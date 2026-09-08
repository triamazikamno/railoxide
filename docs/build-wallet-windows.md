This guide builds the RailOxide desktop wallet binary from source on Windows with Ledger and Trezor support enabled.

The commands below target Windows 11.

## Install System Dependencies

Run the dependency installation commands from PowerShell. Visual Studio Build Tools may request administrator elevation.

```powershell
winget install --id Git.Git --exact --source winget --silent --accept-source-agreements --accept-package-agreements --disable-interactivity
winget install --id Rustlang.Rustup --exact --source winget --silent --accept-source-agreements --accept-package-agreements --disable-interactivity
winget install --id Kitware.CMake --exact --source winget --silent --accept-source-agreements --accept-package-agreements --disable-interactivity
winget install --id LLVM.LLVM --exact --source winget --silent --accept-source-agreements --accept-package-agreements --disable-interactivity
winget install --id Microsoft.VisualStudio.2022.BuildTools --exact --source winget --silent --accept-source-agreements --accept-package-agreements --disable-interactivity --override "--wait --quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended --add Microsoft.VisualStudio.Component.VC.Tools.ARM64 --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64 --add Microsoft.VisualStudio.Component.Windows11SDK.26100 --add Microsoft.VisualStudio.Component.VC.CMake.Project --add Microsoft.VisualStudio.Component.VC.Redist.14.Latest"
```

Refresh the current PowerShell session's `PATH` after installing tools:

```powershell
$env:Path = [Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [Environment]::GetEnvironmentVariable("Path", "User") + ";C:\Program Files\LLVM\bin"
git --version
rustup --version
cmake --version
clang --version
```

## Install Rust 1.97.1

Install the toolchain required by the workspace and add the Windows x64 target:

```powershell
rustup toolchain install 1.97.1
rustup target add x86_64-pc-windows-msvc --toolchain 1.97.1
```

## Clone The Repository

```powershell
git clone https://github.com/triamazikamno/railoxide.git
cd railoxide
```

## Install SQLite For Linking

`libsqlite3-sys` links against `sqlite3.lib` on Windows. The official SQLite DLL ZIP provides `sqlite3.dll` and `sqlite3.def`; use the Visual Studio `lib` tool to generate the import library.

```powershell
$SqliteVersion = "3530200"
$SqliteRoot = "$(Get-Location)\.deps\sqlite-x64"
$HostArch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" }

New-Item -ItemType Directory -Path $SqliteRoot -Force
Invoke-WebRequest -Uri "https://www.sqlite.org/2026/sqlite-dll-win-x64-$SqliteVersion.zip" -OutFile "$SqliteRoot\sqlite-dll-win-x64-$SqliteVersion.zip"
Invoke-WebRequest -Uri "https://www.sqlite.org/2026/sqlite-amalgamation-$SqliteVersion.zip" -OutFile "$SqliteRoot\sqlite-amalgamation-$SqliteVersion.zip"
Expand-Archive -LiteralPath "$SqliteRoot\sqlite-dll-win-x64-$SqliteVersion.zip" -DestinationPath $SqliteRoot -Force
Expand-Archive -LiteralPath "$SqliteRoot\sqlite-amalgamation-$SqliteVersion.zip" -DestinationPath $SqliteRoot -Force

cmd.exe /c "`"C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat`" -arch=x64 -host_arch=$HostArch && lib /MACHINE:X64 /DEF:$SqliteRoot\sqlite3.def /OUT:$SqliteRoot\sqlite3.lib"
```

## Build With Hardware Wallet Support

Set the SQLite paths and run Cargo inside the Visual Studio developer environment:

```powershell
$SqliteVersion = "3530200"
$SqliteRoot = "$(Get-Location)\.deps\sqlite-x64"
$HostArch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" }
$env:Path = [Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [Environment]::GetEnvironmentVariable("Path", "User") + ";C:\Program Files\LLVM\bin"
$env:SQLITE3_LIB_DIR = $SqliteRoot
$env:SQLITE3_INCLUDE_DIR = "$SqliteRoot\sqlite-amalgamation-$SqliteVersion"

cmd.exe /c "`"C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat`" -arch=x64 -host_arch=$HostArch && cargo +1.97.1 check -p wallet --features hardware --target x86_64-pc-windows-msvc"
cmd.exe /c "`"C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat`" -arch=x64 -host_arch=$HostArch && cargo +1.97.1 build --release -p wallet --features hardware --target x86_64-pc-windows-msvc"
```

The wallet binary is written to:

```powershell
target\x86_64-pc-windows-msvc\release\wallet.exe
```

Because this build links to the official SQLite DLL, copy `sqlite3.dll` next to `wallet.exe` before running or distributing it:

```powershell
New-Item -ItemType Directory -Path "target\windows\RailOxide-x86_64-windows" -Force
Copy-Item -LiteralPath "target\x86_64-pc-windows-msvc\release\wallet.exe" -Destination "target\windows\RailOxide-x86_64-windows\wallet.exe" -Force
Copy-Item -LiteralPath "$SqliteRoot\sqlite3.dll" -Destination "target\windows\RailOxide-x86_64-windows\sqlite3.dll" -Force
```

Verify the binary starts far enough to parse command-line options:

```powershell
.\target\windows\RailOxide-x86_64-windows\wallet.exe --help
```

Run the wallet:

```powershell
.\target\windows\RailOxide-x86_64-windows\wallet.exe
```

To store wallet data in a custom location:

```powershell
.\target\windows\RailOxide-x86_64-windows\wallet.exe --db-path "$env:LOCALAPPDATA\RailOxide"
```

Create a redistributable ZIP:

```powershell
Compress-Archive -LiteralPath "target\windows\RailOxide-x86_64-windows" -DestinationPath "target\windows\RailOxide-x86_64-windows.zip" -Force
```

## Hardware Wallet USB Access

The `hardware` feature enables Ledger and Trezor integration through `coins-ledger`, `hidapi-rusb`, and `trezor-client`.

Windows does not need Linux udev rules. If the app builds but cannot open a connected hardware wallet, update the device firmware, close Ledger Live or Trezor Suite if it has exclusive access, reconnect the device, and confirm Windows can see it in Device Manager.

Current hardware-wallet support is hardware-derived software custody. The device is used to derive profile material, while RAILGUN transaction preparation and signing still happen in desktop memory.

## Troubleshooting

If `cargo`, `cmake`, or `clang` is not found after installing tools, refresh `PATH` in the current shell:

```powershell
$env:Path = [Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [Environment]::GetEnvironmentVariable("Path", "User") + ";C:\Program Files\LLVM\bin"
```

If a build script reports `VCINSTALLDIR = None`, `link.exe` is missing, or MSVC libraries are missing, run Cargo through `VsDevCmd.bat` as shown above instead of from a plain shell.

If `ring` fails with `failed to find tool "clang"`, install `LLVM.LLVM` with `winget` and refresh `PATH`.

If the release link fails with `cannot open input file 'sqlite3.lib'`, confirm `sqlite3.lib` was generated from `sqlite3.def`, then set `SQLITE3_LIB_DIR` and `SQLITE3_INCLUDE_DIR` before building.

If `wallet.exe` fails to start because `sqlite3.dll` is missing, copy `sqlite3.dll` into the same directory as `wallet.exe`.

If `aarch64-pc-windows-msvc` fails in `corosensei`, build `--target x86_64-pc-windows-msvc` as shown above. Windows on ARM can run the resulting x64 binary through Windows emulation.

If the GUI fails to open even though `wallet.exe --help` works, update the GPU driver and confirm Vulkan-capable graphics are available.
