param(
    [string]$InstallDir = "",
    [string]$SourceDir = "",
    [switch]$Main,
    [string]$Ref = "",
    [switch]$NoDeps,
    [switch]$NoHardware,
    [switch]$Yes,
    [switch]$DryRun,
    [switch]$NoShortcut,
    [switch]$Help
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoUrl = "https://github.com/triamazikamno/railoxide.git"
$RepoApiUrl = "https://api.github.com/repos/triamazikamno/railoxide"
$Branch = "main"
$RustVersion = "1.97.1"
$Target = "x86_64-pc-windows-msvc"
$Toolchain = "$RustVersion-$Target"
$SqliteVersion = "3530200"
$SourceMode = "release"
$SourceRef = ""
$ExplicitSource = $false
$PythonCommand = ""
$PythonArgs = @()

function Show-Usage {
    @"
RailOxide Windows source installer.

This installer builds RailOxide from source. By default it prompts for the
source to build and defaults to the latest published GitHub release, including
pre-releases. The wallet and extension are built locally; build helpers may be downloaded.

Usage:
  irm https://raw.githubusercontent.com/triamazikamno/railoxide/main/scripts/install-wallet.ps1 | iex

Options:
  -InstallDir PATH       Install wallet.exe and sqlite3.dll under PATH
  -SourceDir PATH        Use PATH for the managed source checkout
  -Main                  Build the latest main branch without prompting
  -Ref REF               Build a specific tag, branch, or commit without prompting
  -NoDeps                Do not install missing system dependencies
  -NoHardware            Build without hardware-wallet support
  -Yes                   Do not prompt before dependency installs/downloads
  -DryRun                Print what would happen without changing anything
  -NoShortcut            Do not create a Start Menu shortcut
  -Help                  Show this help

Inspect-first flow:
  iwr https://raw.githubusercontent.com/triamazikamno/railoxide/main/scripts/install-wallet.ps1 -OutFile install-wallet.ps1
  notepad .\install-wallet.ps1
  powershell -ExecutionPolicy Bypass -File .\install-wallet.ps1

"@
}

function Write-Step {
    param([string]$Message)
    Write-Host "==> $Message"
}

function Stop-Install {
    param([string]$Message)
    throw $Message
}

function Confirm-Action {
    param([string]$Prompt)

    if ($Yes) {
        return $true
    }

    try {
        $answer = Read-Host "$Prompt [Y/n]"
    } catch {
        Stop-Install "$Prompt Pass -Yes to continue non-interactively."
    }

    if ([string]::IsNullOrWhiteSpace($answer)) {
        return $true
    }

    $normalized = $answer.Trim().ToLowerInvariant()
    return $normalized -eq "y" -or $normalized -eq "yes"
}

function Invoke-External {
    param(
        [string]$FilePath,
        [string[]]$ArgumentList = @()
    )

    if ($DryRun) {
        $quotedArgs = $ArgumentList | ForEach-Object {
            if ($_ -match '\s') { '"{0}"' -f $_ } else { $_ }
        }
        Write-Host "+ $FilePath $($quotedArgs -join ' ')"
        return
    }

    & $FilePath @ArgumentList
    if ($LASTEXITCODE -ne 0) {
        Stop-Install "$FilePath exited with status $LASTEXITCODE"
    }
}

function Invoke-Cmd {
    param([string]$Command)
    Invoke-External "cmd.exe" @("/c", $Command)
}

function Test-Command {
    param([string]$Name)
    return $null -ne (Get-Command $Name -ErrorAction SilentlyContinue)
}

function Refresh-Path {
    $machinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $extraPaths = @(
        "C:\Program Files\LLVM\bin",
        "C:\Program Files\Git\cmd",
        "C:\Program Files\CMake\bin"
    )
    $env:Path = (@($machinePath, $userPath) + $extraPaths | Where-Object { $_ }) -join ";"
}

function Get-VsDevCmdPath {
    $programFilesX86 = ${env:ProgramFiles(x86)}
    if ([string]::IsNullOrWhiteSpace($programFilesX86)) {
        $programFilesX86 = Join-Path $env:SystemDrive "Program Files (x86)"
    }

    $candidates = @(
        "$programFilesX86\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat",
        "$programFilesX86\Microsoft Visual Studio\2022\Community\Common7\Tools\VsDevCmd.bat",
        "$programFilesX86\Microsoft Visual Studio\2022\Professional\Common7\Tools\VsDevCmd.bat",
        "$programFilesX86\Microsoft Visual Studio\2022\Enterprise\Common7\Tools\VsDevCmd.bat"
    )

    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate) {
            return $candidate
        }
    }

    return $null
}

function Install-WingetPackage {
    param(
        [string]$Id,
        [string]$Name,
        [string]$Override = ""
    )

    Write-Step "installing $Name with winget"
    $args = @(
        "install",
        "--id", $Id,
        "--exact",
        "--source", "winget",
        "--silent",
        "--accept-source-agreements",
        "--accept-package-agreements",
        "--disable-interactivity"
    )

    if (-not [string]::IsNullOrWhiteSpace($Override)) {
        $args += @("--override", $Override)
    }

    Invoke-External "winget" $args
}

function Ensure-Dependencies {
    Refresh-Path

    $missing = New-Object System.Collections.Generic.List[string]
    if (-not (Test-Command "git")) { $missing.Add("Git") }
    if (-not (Test-Command "rustup")) { $missing.Add("Rustup") }
    if (-not (Test-Command "cmake")) { $missing.Add("CMake") }
    if (-not (Test-Command "clang")) { $missing.Add("LLVM") }
    if (-not (Get-VsDevCmdPath)) { $missing.Add("Visual Studio Build Tools") }

    if ($missing.Count -eq 0) {
        return
    }

    if ($NoDeps) {
        Stop-Install "missing dependencies: $($missing -join ', '); rerun without -NoDeps or install them manually"
    }

    if (-not (Test-Command "winget")) {
        Stop-Install "winget is required for automatic dependency installation"
    }

    if (-not (Confirm-Action "Install missing dependencies with winget: $($missing -join ', ')?")) {
        Stop-Install "dependencies are required to build RailOxide"
    }

    if ($missing -contains "Git") {
        Install-WingetPackage "Git.Git" "Git"
    }
    if ($missing -contains "Rustup") {
        Install-WingetPackage "Rustlang.Rustup" "Rustup"
    }
    if ($missing -contains "CMake") {
        Install-WingetPackage "Kitware.CMake" "CMake"
    }
    if ($missing -contains "LLVM") {
        Install-WingetPackage "LLVM.LLVM" "LLVM"
    }
    if ($missing -contains "Visual Studio Build Tools") {
        $override = "--wait --quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended --add Microsoft.VisualStudio.Component.VC.Tools.ARM64 --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64 --add Microsoft.VisualStudio.Component.Windows11SDK.26100 --add Microsoft.VisualStudio.Component.VC.CMake.Project --add Microsoft.VisualStudio.Component.VC.Redist.14.Latest"
        Install-WingetPackage "Microsoft.VisualStudio.2022.BuildTools" "Visual Studio Build Tools" $override
    }

    Refresh-Path
}

function Ensure-RequiredTools {
    Refresh-Path
    foreach ($tool in @("git", "rustup", "cmake", "clang")) {
        if (-not (Test-Command $tool)) {
            Stop-Install "$tool is required; install it or rerun without -NoDeps"
        }
    }

    if (-not (Get-VsDevCmdPath)) {
        Stop-Install "Visual Studio Build Tools are required; install them or rerun without -NoDeps"
    }
}

function Ensure-RustToolchain {
    Write-Step "installing Rust toolchain $Toolchain"
    $toolchainArgs = @("toolchain", "install", $Toolchain)
    if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") {
        $toolchainArgs += "--force-non-host"
    }

    Invoke-External "rustup" $toolchainArgs
    Invoke-External "rustup" @("target", "add", $Target, "--toolchain", $Toolchain)
}

function Get-ManagedMarkerPath {
    return (Join-Path $SourceDir ".railoxide-installer-managed")
}

function Initialize-SourceOptions {
    if ($Main -and -not [string]::IsNullOrWhiteSpace($Ref)) {
        Stop-Install "-Main and -Ref cannot be combined"
    }

    if ($Main) {
        $script:SourceMode = "main"
        $script:SourceRef = $Branch
        $script:ExplicitSource = $true
        return
    }

    if (-not [string]::IsNullOrWhiteSpace($Ref)) {
        $script:SourceMode = "ref"
        $script:SourceRef = $Ref.Trim()
        $script:ExplicitSource = $true
        return
    }

    $script:SourceMode = "release"
    $script:SourceRef = ""
    $script:ExplicitSource = $false
}

function Resolve-LatestReleaseTag {
    Write-Step "resolving latest RailOxide GitHub release"
    $uri = "$RepoApiUrl/releases?per_page=20"
    try {
        $releases = Invoke-RestMethod -Uri $uri -Headers @{
            Accept = "application/vnd.github+json"
            "X-GitHub-Api-Version" = "2022-11-28"
        }
    } catch {
        Stop-Install "failed to query GitHub releases; rerun with -Main to build the main branch"
    }

    $release = @($releases | Where-Object { -not $_.draft } | Select-Object -First 1)
    if ($release.Count -eq 0 -or [string]::IsNullOrWhiteSpace($release[0].tag_name)) {
        Stop-Install "No RailOxide release found; rerun with -Main to build the main branch."
    }

    return [string]$release[0].tag_name
}

function Select-SourceRef {
    if ($SourceMode -eq "main") {
        $script:SourceRef = $Branch
        Write-Step "using latest $Branch branch"
        return
    }

    if ($SourceMode -eq "ref") {
        if ([string]::IsNullOrWhiteSpace($SourceRef)) {
            Stop-Install "-Ref requires a non-empty value"
        }
        Write-Step "using source ref $SourceRef"
        return
    }

    $latest = Resolve-LatestReleaseTag
    $script:SourceRef = $latest

    if ($ExplicitSource -or $Yes) {
        Write-Step "using latest RailOxide release $SourceRef"
        return
    }

    try {
        Write-Host ""
        Write-Host "RailOxide source to build:"
        Write-Host "  1. Latest release: $latest"
        Write-Host "  2. Latest $Branch branch"
        Write-Host "  3. Specific tag, branch, or commit"
        $answer = Read-Host "Choose source [1]"
    } catch {
        Write-Step "using latest RailOxide release $SourceRef"
        return
    }

    if ([string]::IsNullOrWhiteSpace($answer)) {
        Write-Step "using latest RailOxide release $SourceRef"
        return
    }

    switch ($answer.Trim().ToLowerInvariant()) {
        { $_ -eq "1" -or $_ -eq "r" -or $_ -eq "release" } {
            $script:SourceMode = "release"
            $script:SourceRef = $latest
            Write-Step "using latest RailOxide release $SourceRef"
            return
        }
        { $_ -eq "2" -or $_ -eq "m" -or $_ -eq "main" } {
            $script:SourceMode = "main"
            $script:SourceRef = $Branch
            Write-Step "using latest $Branch branch"
            return
        }
        { $_ -eq "3" -or $_ -eq "ref" -or $_ -eq "custom" } {
            $customRef = Read-Host "Enter tag, branch, or commit"
            if ([string]::IsNullOrWhiteSpace($customRef)) {
                Stop-Install "source ref is required"
            }
            $script:SourceMode = "ref"
            $script:SourceRef = $customRef.Trim()
            Write-Step "using source ref $SourceRef"
            return
        }
        default {
            Stop-Install "invalid source selection: $answer"
        }
    }
}

function Test-GitRef {
    param([string]$RefName)
    & git -C $SourceDir rev-parse --verify --quiet $RefName | Out-Null
    return $LASTEXITCODE -eq 0
}

function Fetch-SourceRefs {
    Invoke-External "git" @("-C", $SourceDir, "remote", "set-url", "origin", $RepoUrl)
    Invoke-External "git" @("-C", $SourceDir, "remote", "set-branches", "origin", "*")

    if ($SourceMode -eq "main") {
        Write-Step "fetching latest $Branch branch"
        Invoke-External "git" @("-C", $SourceDir, "fetch", "--prune", "origin", $Branch)
        return
    }

    Write-Step "fetching source refs and tags"
    Invoke-External "git" @("-C", $SourceDir, "fetch", "--prune", "--tags", "origin")
}

function Checkout-MainBranch {
    if (Test-GitRef "refs/heads/$Branch") {
        Invoke-External "git" @("-C", $SourceDir, "checkout", $Branch)
        Invoke-External "git" @("-C", $SourceDir, "merge", "--ff-only", "origin/$Branch")
    } else {
        Invoke-External "git" @("-C", $SourceDir, "checkout", "-b", $Branch, "origin/$Branch")
    }
}

function Checkout-ReleaseRef {
    if (-not (Test-GitRef "refs/tags/$SourceRef^{commit}")) {
        Stop-Install "release tag $SourceRef was not found after fetching tags"
    }
    Invoke-External "git" @("-C", $SourceDir, "checkout", "--detach", "refs/tags/$SourceRef")
}

function Checkout-CustomRef {
    if (Test-GitRef "$SourceRef^{commit}") {
        Invoke-External "git" @("-C", $SourceDir, "checkout", "--detach", $SourceRef)
    } elseif (Test-GitRef "origin/$SourceRef^{commit}") {
        Invoke-External "git" @("-C", $SourceDir, "checkout", "--detach", "origin/$SourceRef")
    } else {
        Stop-Install "source ref $SourceRef was not found after fetching refs"
    }
}

function Checkout-SourceRef {
    switch ($SourceMode) {
        "main" { Checkout-MainBranch; return }
        "release" { Checkout-ReleaseRef; return }
        "ref" { Checkout-CustomRef; return }
        default { Stop-Install "unknown source mode: $SourceMode" }
    }
}

function Ensure-SourceCheckout {
    $marker = Get-ManagedMarkerPath
    $parent = Split-Path -Parent $SourceDir

    if (-not (Test-Path -LiteralPath $SourceDir)) {
        Write-Step "cloning RailOxide into $SourceDir"
        if ($DryRun) {
            Write-Host "+ New-Item -ItemType Directory -Force $parent"
            Write-Host "+ git clone $RepoUrl $SourceDir"
            Write-Host "+ write installer marker $marker"
        } else {
            New-Item -ItemType Directory -Path $parent -Force | Out-Null
            Invoke-External "git" @("clone", $RepoUrl, $SourceDir)
            Set-Content -LiteralPath $marker -Value $RepoUrl -Encoding ASCII
        }
    } elseif (-not (Test-Path -LiteralPath $marker)) {
        Stop-Install "$SourceDir already exists and is not marked as installer-managed; pass -SourceDir to use another path"
    } elseif (-not (Test-Path -LiteralPath (Join-Path $SourceDir ".git"))) {
        Stop-Install "$SourceDir exists but is not a Git checkout"
    } else {
        $status = (& git -C $SourceDir status --porcelain --untracked-files=no)
        if ($LASTEXITCODE -ne 0) {
            Stop-Install "git status failed in $SourceDir"
        }
        if (-not [string]::IsNullOrWhiteSpace(($status -join "`n"))) {
            Stop-Install "$SourceDir has local modifications; resolve them or use -SourceDir with a fresh path"
        }
    }

    Fetch-SourceRefs
    Checkout-SourceRef
}

function Get-SourceCommit {
    $commit = (& git -C $SourceDir rev-parse HEAD)
    if ($LASTEXITCODE -ne 0) {
        Stop-Install "git rev-parse failed in $SourceDir"
    }
    return $commit.Trim()
}

function Ensure-SqliteForLinking {
    $sqliteRoot = Join-Path $SourceDir ".deps\sqlite-x64"
    $dllZip = Join-Path $sqliteRoot "sqlite-dll-win-x64-$SqliteVersion.zip"
    $amalgamationZip = Join-Path $sqliteRoot "sqlite-amalgamation-$SqliteVersion.zip"
    $dllUrl = "https://www.sqlite.org/2026/sqlite-dll-win-x64-$SqliteVersion.zip"
    $amalgamationUrl = "https://www.sqlite.org/2026/sqlite-amalgamation-$SqliteVersion.zip"
    $sqliteLib = Join-Path $sqliteRoot "sqlite3.lib"
    $sqliteDef = Join-Path $sqliteRoot "sqlite3.def"

    Write-Step "preparing SQLite $SqliteVersion for Windows linking"
    if ($DryRun) {
        Write-Host "+ New-Item -ItemType Directory -Force $sqliteRoot"
        Write-Host "+ Invoke-WebRequest $dllUrl -OutFile $dllZip"
        Write-Host "+ Invoke-WebRequest $amalgamationUrl -OutFile $amalgamationZip"
        Write-Host "+ Expand-Archive $dllZip -DestinationPath $sqliteRoot -Force"
        Write-Host "+ Expand-Archive $amalgamationZip -DestinationPath $sqliteRoot -Force"
    } else {
        New-Item -ItemType Directory -Path $sqliteRoot -Force | Out-Null
        Invoke-WebRequest -Uri $dllUrl -OutFile $dllZip
        Invoke-WebRequest -Uri $amalgamationUrl -OutFile $amalgamationZip
        Expand-Archive -LiteralPath $dllZip -DestinationPath $sqliteRoot -Force
        Expand-Archive -LiteralPath $amalgamationZip -DestinationPath $sqliteRoot -Force
    }

    $vsDevCmd = Get-VsDevCmdPath
    if (-not $vsDevCmd) {
        Stop-Install "Visual Studio Build Tools are required to generate sqlite3.lib"
    }

    $hostArch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" }
    $command = '"{0}" -arch=x64 -host_arch={1} && lib /MACHINE:X64 /DEF:"{2}" /OUT:"{3}"' -f $vsDevCmd, $hostArch, $sqliteDef, $sqliteLib
    Invoke-Cmd $command

    $env:SQLITE3_LIB_DIR = $sqliteRoot
    $env:SQLITE3_INCLUDE_DIR = Join-Path $sqliteRoot "sqlite-amalgamation-$SqliteVersion"
}

function Find-Python {
    foreach ($name in @("py", "python", "python3")) {
        if (-not (Test-Command $name)) { continue }
        $prefixArgs = if ($name -eq "py") { @("-3") } else { @() }
        try {
            & $name @prefixArgs -c "import sys; sys.exit(sys.version_info < (3, 9))" 2>$null
        } catch {
            continue
        }
        if ($LASTEXITCODE -eq 0) {
            $script:PythonCommand = $name
            $script:PythonArgs = $prefixArgs
            return $true
        }
    }
    return $false
}

function Ensure-ExtensionDependencies {
    if (-not (Test-Path -LiteralPath (Join-Path $SourceDir "scripts\build-browser-extension"))) {
        Write-Step "selected source predates the browser extension; skipping extension tools"
        return
    }
    if (-not (Find-Python)) {
        if ($NoDeps) { Stop-Install "Python 3 is required for the browser extension; -NoDeps was provided" }
        if (-not (Confirm-Action "Install Python 3 with winget for the browser extension?")) {
            Stop-Install "Python 3 is required"
        }
        Install-WingetPackage "Python.Python.3.13" "Python 3"
        Refresh-Path
        if (-not (Find-Python)) { Stop-Install "Python 3 is still unavailable; reopen PowerShell and rerun" }
    }
    Invoke-External "rustup" @("target", "add", "wasm32-unknown-unknown", "--toolchain", $Toolchain)
    $matchingBindgen = $false
    if (Test-Command "wasm-bindgen") {
        $version = & wasm-bindgen --version
        $matchingBindgen = $LASTEXITCODE -eq 0 -and $version -eq "wasm-bindgen 0.2.126"
    }
    if (-not $matchingBindgen) {
        if ($NoDeps) { Stop-Install "wasm-bindgen 0.2.126 is required; -NoDeps was provided" }
        if (-not (Confirm-Action "Download the checksum-pinned wasm-bindgen 0.2.126 build helper?")) {
            Stop-Install "wasm-bindgen is required"
        }
        $toolsDir = Join-Path $SourceDir "target\browser-extension-tools"
        Invoke-External $PythonCommand ($PythonArgs + @((Join-Path $SourceDir "scripts\install-browser-extension-tools.py"), $toolsDir))
        $env:Path = (Join-Path $toolsDir "bin") + ";" + $env:Path
    }
}

function Build-BrowserExtension {
    $metadataPath = [IO.Path]::GetTempFileName()
    Push-Location $SourceDir
    try {
        $vsDevCmd = Get-VsDevCmdPath
        $hostArch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" }
        $previousBootstrap = $env:RUSTC_BOOTSTRAP
        try {
            # wasm_thread needs stdarch_wasm_atomic_wait only during WASM compilation.
            $env:RUSTC_BOOTSTRAP = "1"
            $command = '"{0}" -arch=x64 -host_arch={1} && cargo +{2} build -p browser-frontend -p dapp-gateway-protocol --target wasm32-unknown-unknown --release --locked' -f $vsDevCmd, $hostArch, $Toolchain
            Invoke-Cmd $command
        } finally {
            $env:RUSTC_BOOTSTRAP = $previousBootstrap
        }
        # Preserve Cargo's UTF-8 bytes, including Unicode paths, without PowerShell transcoding or a BOM.
        Invoke-Cmd ('cargo +{0} metadata --format-version 1 --locked --filter-platform wasm32-unknown-unknown > "{1}"' -f $Toolchain, $metadataPath)
        Invoke-External $PythonCommand ($PythonArgs + @("scripts\package-browser-extension.py", $metadataPath))
    } finally {
        Pop-Location
        Remove-Item -LiteralPath $metadataPath -Force
    }
}

function Build-Wallet {
    $features = if ($NoHardware) { "" } else { "hardware" }
    $featureText = if ([string]::IsNullOrWhiteSpace($features)) { "none" } else { $features }
    Write-Step "building RailOxide wallet release binary with features: $featureText"

    $vsDevCmd = Get-VsDevCmdPath
    if (-not $vsDevCmd) {
        Stop-Install "Visual Studio Build Tools are required to build RailOxide"
    }

    $manifestPath = Join-Path $SourceDir "Cargo.toml"
    $cargoArgs = @("+$Toolchain", "build", "--release", "--manifest-path", $manifestPath, "-p", "wallet")
    if (-not [string]::IsNullOrWhiteSpace($features)) {
        $cargoArgs += @("--features", $features)
    }
    $cargoArgs += @("--target", $Target)

    $hostArch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" }
    $cargoCommand = ($cargoArgs | ForEach-Object {
        if ($_ -match '\s') { '"{0}"' -f $_ } else { $_ }
    }) -join " "
    $command = '"{0}" -arch=x64 -host_arch={1} && cargo {2}' -f $vsDevCmd, $hostArch, $cargoCommand
    $hasExtension = Test-Path -LiteralPath (Join-Path $SourceDir "scripts\build-browser-extension")
    $previousBundle = $env:RAILOXIDE_EXTENSION_BUNDLE
    try {
        if ($hasExtension) {
            Build-BrowserExtension
            $env:RAILOXIDE_EXTENSION_BUNDLE = Join-Path $SourceDir "target\browser-extension.zip"
            if (-not (Test-Path -LiteralPath $env:RAILOXIDE_EXTENSION_BUNDLE) -or (Get-Item -LiteralPath $env:RAILOXIDE_EXTENSION_BUNDLE).Length -eq 0) {
                Stop-Install "extension build did not produce a ZIP"
            }
        }
        Invoke-Cmd $command
        if ($hasExtension) {
            $walletExe = Join-Path $SourceDir "target\$Target\release\wallet.exe"
            Invoke-External $PythonCommand ($PythonArgs + @((Join-Path $SourceDir "scripts\verify-browser-extension.py"), $walletExe, $env:RAILOXIDE_EXTENSION_BUNDLE))
        }
    } finally {
        $env:RAILOXIDE_EXTENSION_BUNDLE = $previousBundle
    }
}

function Install-WalletFiles {
    $sqliteRoot = Join-Path $SourceDir ".deps\sqlite-x64"
    $walletExe = Join-Path $SourceDir "target\$Target\release\wallet.exe"
    $sqliteDll = Join-Path $sqliteRoot "sqlite3.dll"
    $iconSrc = Join-Path $SourceDir "bins\wallet\packaging\icons\windows\RailOxide.ico"

    Write-Step "installing RailOxide to $InstallDir"
    if ($DryRun) {
        Write-Host "+ New-Item -ItemType Directory -Force $InstallDir"
        Write-Host "+ Copy-Item $walletExe $InstallDir\wallet.exe"
        Write-Host "+ Copy-Item $sqliteDll $InstallDir\sqlite3.dll"
        Write-Host "+ Copy-Item $iconSrc $InstallDir\RailOxide.ico"
    } else {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        Copy-Item -LiteralPath $walletExe -Destination (Join-Path $InstallDir "wallet.exe") -Force
        Copy-Item -LiteralPath $sqliteDll -Destination (Join-Path $InstallDir "sqlite3.dll") -Force
        if (Test-Path -LiteralPath $iconSrc) {
            Copy-Item -LiteralPath $iconSrc -Destination (Join-Path $InstallDir "RailOxide.ico") -Force
        }
    }
}

function Install-Shortcut {
    if ($NoShortcut) {
        return
    }

    $startMenuDir = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs"
    $shortcutPath = Join-Path $startMenuDir "RailOxide.lnk"
    $walletExe = Join-Path $InstallDir "wallet.exe"
    $iconPath = Join-Path $InstallDir "RailOxide.ico"

    Write-Step "creating Start Menu shortcut"
    if ($DryRun) {
        Write-Host "+ create shortcut $shortcutPath -> $walletExe"
        return
    }

    $shell = New-Object -ComObject WScript.Shell
    New-Item -ItemType Directory -Path $startMenuDir -Force | Out-Null
    $shortcut = $shell.CreateShortcut($shortcutPath)
    $shortcut.TargetPath = $walletExe
    $shortcut.WorkingDirectory = $InstallDir
    if (Test-Path -LiteralPath $iconPath) {
        $shortcut.IconLocation = $iconPath
    }
    $shortcut.Save()
}

function Invoke-SmokeTest {
    $walletExe = Join-Path $InstallDir "wallet.exe"
    Write-Step "verifying installed wallet command parses --help"
    Invoke-External $walletExe @("--help")
}

function Show-DryRunPlan {
    $sourceModeText = switch ($SourceMode) {
        "release" { "latest release"; break }
        "main" { "main branch"; break }
        "ref" { "specific ref"; break }
        default { $SourceMode; break }
    }
    $sourceRefText = switch ($SourceMode) {
        "release" { "resolved during install"; break }
        "main" { $Branch; break }
        "ref" { $SourceRef; break }
        default { if ([string]::IsNullOrWhiteSpace($SourceRef)) { "unknown" } else { $SourceRef }; break }
    }

    Write-Step "dry run: no commands will be executed"
    Write-Host "repository: $RepoUrl"
    Write-Host "source mode: $sourceModeText"
    Write-Host "source ref: $sourceRefText"
    Write-Host "toolchain: $Toolchain"
    Write-Host "target: $Target"
    Write-Host "install directory: $InstallDir"
    Write-Host "source checkout: $SourceDir"
    Write-Host "system dependencies: $(-not $NoDeps)"
    Write-Host "hardware support: $(-not $NoHardware)"
    Write-Host "browser extension: build and embed when supported by the selected source; requires Python 3, the WASM target, and pinned wasm-bindgen"
    Write-Host "Start Menu shortcut: $(-not $NoShortcut)"
}

function Main {
    if ($Help) {
        Show-Usage
        return
    }

    if ($env:OS -ne "Windows_NT") {
        Stop-Install "unsupported OS: this installer requires Windows"
    }

    if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
        Stop-Install "LOCALAPPDATA is not set"
    }
    if ([string]::IsNullOrWhiteSpace($env:APPDATA)) {
        Stop-Install "APPDATA is not set"
    }

    if ([string]::IsNullOrWhiteSpace($InstallDir)) {
        $script:InstallDir = Join-Path $env:LOCALAPPDATA "RailOxide\bin"
    }
    if ([string]::IsNullOrWhiteSpace($SourceDir)) {
        $script:SourceDir = Join-Path $env:LOCALAPPDATA "RailOxide\src\railoxide"
    }

    Initialize-SourceOptions

    if ($DryRun) {
        Show-DryRunPlan
        return
    }

    Ensure-Dependencies
    Ensure-RequiredTools
    Select-SourceRef
    Ensure-RustToolchain
    Ensure-SourceCheckout
    Write-Step "building source commit $(Get-SourceCommit)"
    Ensure-SqliteForLinking
    Ensure-ExtensionDependencies
    Build-Wallet
    Install-WalletFiles
    Install-Shortcut
    Invoke-SmokeTest

    Write-Step "installed RailOxide to $InstallDir"
    Write-Step "done"
}

Main
