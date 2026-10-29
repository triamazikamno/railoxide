#!/usr/bin/env python3
"""Windows release packaging helpers: wallet version, ZIP packaging, DLL import checks and Scoop manifest."""

import argparse
import fnmatch
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import zipfile


ROOT = Path(__file__).resolve().parents[1]
TARGET = 'x86_64-pc-windows-msvc'
MANIFEST_TEMPLATE = ROOT / 'bins/wallet/packaging/windows/railoxide.json.in'
RELEASE_DIR = ROOT / 'target/windows/release'
EXTENSION_BUNDLE = ROOT / 'target/browser-extension.zip'
PACKAGE_FILES = {
    'wallet.exe': ROOT / f'target/{TARGET}/release/wallet.exe',
    'RailOxide.ico': ROOT / 'bins/wallet/packaging/icons/windows/RailOxide.ico',
    'LICENSE': ROOT / 'LICENSE',
    'libusb-LICENSE-LGPL-2.1.txt': ROOT / 'bins/wallet/packaging/macos/libusb-LICENSE-LGPL-2.1.txt',
}
# Visual C++ redistributable DLLs. Checked before the allowlist so a mistaken entry can't admit one.
REDISTRIBUTABLE_PATTERNS = (
    'vcruntime*', 'msvcp*', 'msvcr*', 'concrt*', 'vccorlib*', 'vcomp*', 'vcamp*', 'mfc*',
    'api-ms-win-crt-*', 'ucrtbase*',
)
# DLLs and API set contracts that ship with x64 Windows 11 desktop. Add a name only after
# confirming that; the build runner's System32 also holds redistributables.
SYSTEM_DLLS = frozenset({
    'advapi32.dll', 'bcrypt.dll', 'bcryptprimitives.dll', 'cfgmgr32.dll', 'comctl32.dll',
    'comdlg32.dll', 'crypt32.dll', 'd2d1.dll', 'd3d11.dll', 'd3d12.dll', 'd3dcompiler_47.dll',
    'dcomp.dll', 'dwmapi.dll', 'dwrite.dll', 'dxgi.dll', 'gdi32.dll', 'hid.dll', 'imm32.dll',
    'iphlpapi.dll', 'kernel32.dll', 'ncrypt.dll', 'ntdll.dll', 'ole32.dll', 'oleaut32.dll',
    'opengl32.dll', 'propsys.dll', 'rpcrt4.dll', 'secur32.dll', 'setupapi.dll', 'shcore.dll',
    'shell32.dll', 'shlwapi.dll', 'user32.dll', 'userenv.dll', 'uxtheme.dll', 'version.dll',
    'windowscodecs.dll', 'winmm.dll', 'wintrust.dll', 'winusb.dll', 'ws2_32.dll',
    # Imported by windows-result through the pinned GPUI.
    'api-ms-win-core-winrt-error-l1-1-0.dll',
    # WaitOnAddress, used by Rust std.
    'api-ms-win-core-synch-l1-2-0.dll',
})
DEPENDENCY_HEADERS = (
    'Image has the following dependencies:',
    'Image has the following delay load dependencies:',
)


def load_macos_helpers():
    spec = importlib.util.spec_from_file_location('macos_packaging', ROOT / 'scripts/macos-packaging.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


macos = load_macos_helpers()
PackagingError = macos.PackagingError


def wallet_version(manifest=macos.MANIFEST):
    return macos.checked_version(macos.manifest_version(manifest)).group(0)


def package_name(version):
    return f'RailOxide-v{version}-windows-x64'


def checksums_name(version):
    return f'RailOxide-v{version}-windows-SHA256SUMS'


def command_version(args):
    print(wallet_version(args.manifest))


def run(command, **kwargs):
    print('+ ' + ' '.join(map(str, command)), flush=True)
    result = subprocess.run(command, cwd=ROOT, **kwargs)
    if result.returncode != 0:
        raise PackagingError(f'{command[0]} exited with status {result.returncode}')


def build_extension(cargo):
    bindgen = shutil.which('wasm-bindgen')
    if bindgen is None or subprocess.run(
        [bindgen, '--version'], capture_output=True, text=True,
    ).stdout.strip() != 'wasm-bindgen 0.2.126':
        raise PackagingError('expected exactly wasm-bindgen 0.2.126 on PATH; see docs/browser-extension.md')
    # wasm_thread requires the unstable stdarch_wasm_atomic_wait feature on WASM.
    run(
        cargo + ['build', '-p', 'browser-frontend', '-p', 'dapp-gateway-protocol',
                 '--target', 'wasm32-unknown-unknown', '--release', '--locked'],
        env={**os.environ, 'RUSTC_BOOTSTRAP': '1'},
    )
    with tempfile.TemporaryDirectory() as temporary:
        metadata = Path(temporary) / 'metadata.json'
        with open(metadata, 'wb') as output:
            run(cargo + ['metadata', '--format-version', '1', '--locked',
                         '--filter-platform', 'wasm32-unknown-unknown'], stdout=output)
        run([sys.executable, ROOT / 'scripts/package-browser-extension.py', metadata])
    if not EXTENSION_BUNDLE.is_file() or EXTENSION_BUNDLE.stat().st_size == 0:
        raise PackagingError('extension build did not produce a ZIP')


def write_package(version, files, release_dir):
    name = package_name(version)
    archive_name = f'{name}.zip'
    shutil.rmtree(release_dir, ignore_errors=True)
    release_dir.mkdir(parents=True)
    with zipfile.ZipFile(release_dir / archive_name, 'w', zipfile.ZIP_DEFLATED) as archive:
        for entry, source in files.items():
            archive.write(source, f'{name}/{entry}')
    digest = hashlib.sha256((release_dir / archive_name).read_bytes()).hexdigest()
    (release_dir / checksums_name(version)).write_bytes(f'{digest}  {archive_name}\n'.encode())
    return release_dir / archive_name


def command_package(args):
    version = wallet_version()
    for tool in ('cl', 'link'):
        if shutil.which(tool) is None:
            raise PackagingError(f'{tool} is not on PATH; run inside a Visual Studio developer environment')
    # Either variable replaces the target-specific flags below, dropping the static CRT.
    for variable in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS'):
        if os.environ.get(variable):
            raise PackagingError(f'{variable} is set and would override the static CRT flag')
    toolchain = os.environ.get('CARGO_TOOLCHAIN')
    cargo = ['cargo'] + ([f'+{toolchain}'] if toolchain else [])

    build_extension(cargo)
    run(
        cargo + ['build', '--release', '--locked', '-p', 'wallet', '--features', 'hardware', '--target', TARGET],
        env={
            **os.environ,
            'RAILOXIDE_EXTENSION_BUNDLE': str(EXTENSION_BUNDLE),
            # Target-specific, so the WASM extension build and host build scripts keep the default.
            'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS': '-C target-feature=+crt-static',
        },
    )
    run([sys.executable, ROOT / 'scripts/verify-browser-extension.py',
         PACKAGE_FILES['wallet.exe'], EXTENSION_BUNDLE])
    archive = write_package(version, PACKAGE_FILES, RELEASE_DIR)
    print(f'packaged {archive}')
    print(f'packaged {RELEASE_DIR / checksums_name(version)}')


def parse_dependents(output):
    """Return the normal and delay-load DLL names from `dumpbin /dependents` output."""
    names = []
    in_section = False
    found_header = False
    for line in output.splitlines():
        stripped = line.strip()
        if stripped in DEPENDENCY_HEADERS:
            in_section = True
            found_header = True
            section_started = False
        elif stripped == 'Summary':
            break
        elif in_section and not stripped:
            # A blank line follows the header; the next one ends the list.
            in_section = not section_started
        elif in_section:
            names.append(stripped)
            section_started = True
    if not found_header:
        raise PackagingError('dumpbin output has no dependency section')
    return names


def disallowed_imports(names, allowlist=SYSTEM_DLLS):
    allowed = {name.lower() for name in allowlist}
    return [
        name for name in names
        if any(fnmatch.fnmatchcase(name.lower(), pattern) for pattern in REDISTRIBUTABLE_PATTERNS)
        or name.lower() not in allowed
    ]


def pe_files(paths):
    for root in map(Path, paths):
        if root.is_file():
            yield root
        elif root.is_dir():
            yield from sorted(
                path for path in root.rglob('*')
                if path.is_file() and path.suffix.lower() in ('.exe', '.dll')
            )
        else:
            raise PackagingError(f'not a file or directory: {root}')


def dumpbin_dependents(path):
    try:
        result = subprocess.run(['dumpbin', '/nologo', '/dependents', str(path)], capture_output=True, text=True)
    except FileNotFoundError as error:
        raise PackagingError('dumpbin is required; run inside a Visual Studio developer environment') from error
    if result.returncode != 0:
        raise PackagingError(f'dumpbin failed for {path}: {result.stdout.strip()} {result.stderr.strip()}')
    return result.stdout


def command_check_imports(args):
    files = list(pe_files(args.paths))
    if not files:
        raise PackagingError('no .exe or .dll files found')
    offending = [
        (path, name)
        for path in files
        for name in disallowed_imports(parse_dependents(dumpbin_dependents(path)))
    ]
    for path, name in offending:
        print(f'{path}: {name}', file=sys.stderr)
    if offending:
        raise PackagingError(f'{len(offending)} DLL import(s) not supplied by Windows')
    print(f'imports ok: {len(files)} file(s) import only Windows system DLLs')


def command_render_manifest(args):
    version = macos.checked_version(args.version).group(0)
    digest = macos.dmg_digest(args.checksums, f'{package_name(version)}.zip')
    manifest = MANIFEST_TEMPLATE.read_text().replace('@VERSION@', version).replace('@SHA256@', digest)
    leftover = re.search(r'@[A-Za-z0-9_]+@', manifest)
    if leftover is not None:
        raise PackagingError(f'unrendered manifest placeholder: {leftover.group(0)}')
    json.loads(manifest)
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(manifest.encode())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)

    version = commands.add_parser('version', help='print the wallet version')
    version.add_argument('--manifest', default=macos.MANIFEST)
    version.set_defaults(handler=command_version)

    package = commands.add_parser('package', help='build the release ZIP and checksum file')
    package.set_defaults(handler=command_package)

    imports = commands.add_parser('check-imports', help='reject DLL imports Windows does not supply')
    imports.add_argument('paths', nargs='+')
    imports.set_defaults(handler=command_check_imports)

    manifest = commands.add_parser('render-manifest', help='render the Scoop manifest')
    manifest.add_argument('--version', required=True)
    manifest.add_argument('--checksums', required=True)
    manifest.add_argument('--output', required=True)
    manifest.set_defaults(handler=command_render_manifest)

    args = parser.parse_args()
    try:
        args.handler(args)
    except PackagingError as error:
        print(f'error: {error}', file=sys.stderr)
        sys.exit(1)


if __name__ == '__main__':
    main()
