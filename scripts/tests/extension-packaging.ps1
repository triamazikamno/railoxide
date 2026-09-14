$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$installer = [IO.File]::ReadAllText((Join-Path $repoRoot "scripts\install-wallet.ps1"))
# Load the installer functions without running installation or dependency setup.
. ([scriptblock]::Create(($installer -replace '(?m)^Main\s*$', '')))

# A launcher can exist on PATH but fail to start an interpreter.
function py { throw "Python launcher unavailable" }
function python { $global:LASTEXITCODE = 0 }
try {
    if (-not (Find-Python) -or $PythonCommand -ne "python") {
        throw "Python discovery did not continue after a failed launcher"
    }
} finally {
    Remove-Item Function:py, Function:python
}

$SourceDir = Join-Path ([IO.Path]::GetTempPath()) ("extension build " + [guid]::NewGuid())
$previousBundle = $env:RAILOXIDE_EXTENSION_BUNDLE
$previousBootstrap = $env:RUSTC_BOOTSTRAP
$PythonCommand = "python"
$PythonArgs = @()
$script:events = @()
$script:failExtension = $false

function Get-VsDevCmdPath { "C:\VS tools\VsDevCmd.bat" }
function Invoke-Cmd {
    param([string]$Command)
    if ($Command -like '*-p browser-frontend*') {
        if ($env:RUSTC_BOOTSTRAP -ne "1") { throw "WASM build missing bootstrap" }
        $script:events += "wasm"
        if ($script:failExtension) { throw "extension build failure" }
    } elseif ($Command -like '* metadata *') {
        if ($env:RUSTC_BOOTSTRAP -ne "original") { throw "bootstrap leaked into metadata" }
        if ($Command -notmatch '> "([^"]+)"$') { throw "metadata must bypass PowerShell transcoding" }
        $json = '{"metadata":"Unicode survives: ' + [char]0x00e5 + '"}'
        [IO.File]::WriteAllText($Matches[1], $json, (New-Object Text.UTF8Encoding($false)))
    } else {
        if ($env:RUSTC_BOOTSTRAP -ne "original") { throw "bootstrap leaked into wallet build" }
        if ($env:RAILOXIDE_EXTENSION_BUNDLE -ne (Join-Path $SourceDir "target\browser-extension.zip")) { throw "wallet uses the wrong bundle" }
        $script:events += "wallet"
    }
}
function Invoke-External {
    param([string]$FilePath, [string[]]$ArgumentList)
    if ($ArgumentList[0] -like '*package-browser-extension.py') {
        $metadataBytes = [IO.File]::ReadAllBytes($ArgumentList[1])
        if ($metadataBytes[0] -ne 123) { throw "metadata has a BOM" }
        $metadata = [Text.Encoding]::UTF8.GetString($metadataBytes) | ConvertFrom-Json
        if ($metadata.metadata -ne ("Unicode survives: " + [char]0x00e5)) { throw "metadata encoding changed" }
        [IO.File]::WriteAllText((Join-Path $SourceDir "target\browser-extension.zip"), "bundle")
        $script:events += "package"
    } elseif ($ArgumentList[0] -like '*verify-browser-extension.py') {
        $script:events += "verify"
    } else {
        throw "unexpected external command: $FilePath"
    }
}

try {
    New-Item -ItemType Directory -Path (Join-Path $SourceDir "scripts"), (Join-Path $SourceDir "target") | Out-Null
    New-Item -ItemType File -Path (Join-Path $SourceDir "scripts\build-browser-extension") | Out-Null
    $env:RAILOXIDE_EXTENSION_BUNDLE = "original bundle"
    $env:RUSTC_BOOTSTRAP = "original"
    Build-Wallet
    if (($script:events -join ',') -ne 'wasm,package,wallet,verify') { throw "incorrect build order" }
    if ($env:RAILOXIDE_EXTENSION_BUNDLE -ne "original bundle" -or $env:RUSTC_BOOTSTRAP -ne "original") { throw "environment was not restored" }

    $script:events = @()
    $script:failExtension = $true
    $failed = $false
    try { Build-Wallet } catch {
        if ($_.Exception.Message -ne "extension build failure") { throw }
        $failed = $true
    }
    if (-not $failed -or ($script:events -join ',') -ne 'wasm') { throw "wallet build continued after extension failure" }
    if ($env:RAILOXIDE_EXTENSION_BUNDLE -ne "original bundle" -or $env:RUSTC_BOOTSTRAP -ne "original") { throw "failure leaked environment" }
    Write-Host "Windows extension packaging checks passed"
} finally {
    $env:RAILOXIDE_EXTENSION_BUNDLE = $previousBundle
    $env:RUSTC_BOOTSTRAP = $previousBootstrap
    Remove-Item -LiteralPath $SourceDir -Recurse -Force
}
