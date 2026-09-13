#!/usr/bin/env python3
"""Stage locked browser assets and web bindings. Called by build-browser-extension."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parent.parent


def package_protocol_licenses(metadata, stage):
    """Preserve notices from the locked protocol dependency closure, including build deps."""
    packages = {package['id']: package for package in metadata['packages']}
    roots = [key for key, package in packages.items() if package['name'] == 'dapp-gateway-protocol']
    if len(roots) != 1 or not metadata.get('resolve'):
        raise RuntimeError('Expected the locked protocol dependency graph')
    nodes = {node['id']: node for node in metadata['resolve']['nodes']}
    pending = roots[:]
    visited = set()
    while pending:
        key = pending.pop()
        if key in visited:
            continue
        visited.add(key)
        pending.extend(nodes[key]['dependencies'])
    notices = stage / 'licenses/protocol'
    notices.mkdir(parents=True)
    index = ['# Protocol dependency notices', '',
             'Packages selected from the locked WASM Cargo dependency graph.',
             'Build dependencies are included conservatively.', '']
    for key in sorted(visited, key=lambda key: (packages[key]['name'], packages[key]['version'])):
        package = packages[key]
        source = Path(package['manifest_path']).parent
        files = set()
        if package.get('license_file'):
            files.add((source / package['license_file']).resolve())
        for child in source.iterdir():
            name = child.name.lower()
            if name.startswith(('license', 'copying', 'notice', 'copyright')):
                if child.is_file():
                    files.add(child)
                elif child.is_dir():
                    files.update(path for path in child.rglob('*') if path.is_file())
        if not files and key in metadata['workspace_members']:
            files.add(ROOT / 'LICENSE')
        if not files or any(not path.is_file() for path in files):
            raise RuntimeError(f"Missing license/notice source for {package['name']} {package['version']}")
        directory = notices / f"{package['name']}-{package['version']}"
        directory.mkdir()
        index.append(f"- {package['name']} {package['version']}: {package.get('license') or 'see notices'}")
        for number, path in enumerate(sorted(files)):
            # Prefix avoids collisions between nested notices without disclosing local paths.
            target = directory / f'{number:02d}-{path.name}'
            shutil.copy2(path, target)
    (notices / 'SOURCES.md').write_text('\n'.join(index) + '\n', encoding='utf-8')


def write_archive(output, files):
    """Write a deterministic zip beside the staged directory, extension files at the archive root."""
    archive = output.with_suffix('.zip')
    if archive.is_symlink():
        raise RuntimeError('Refusing to replace a symlink at target/browser-extension.zip')
    with tempfile.TemporaryDirectory(prefix='browser-extension-zip-', dir=archive.parent) as temporary:
        staged = Path(temporary) / 'bundle.zip'
        with zipfile.ZipFile(staged, 'w', zipfile.ZIP_DEFLATED) as bundle:
            for name in sorted(path.relative_to(output).as_posix() for path in files):
                # A fixed timestamp keeps identical inputs byte-identical across builds.
                entry = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
                entry.compress_type = zipfile.ZIP_DEFLATED
                bundle.writestr(entry, (output / name).read_bytes())
        staged.replace(archive)
    return archive


def package(metadata_path):
    metadata = json.loads(Path(metadata_path).read_text(encoding='utf-8'))
    packages = [p for p in metadata['packages'] if p['name'] == 'gpui-kit-assets' and p['version'] == '0.6.0']
    if len(packages) != 1:
        raise RuntimeError('Expected exactly one locked gpui-kit-assets 0.6.0 source')
    package_source = Path(packages[0]['manifest_path']).parent
    wasm = Path(metadata['target_directory']) / 'wasm32-unknown-unknown/release/browser_frontend.wasm'
    if not wasm.is_file():
        raise RuntimeError('Missing release WASM. Run scripts/build-browser-extension first.')
    # A successful build replaces the entire output, so stale resources cannot linger.
    output = ROOT / 'target/browser-extension'
    output.parent.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='browser-extension-', dir=output.parent) as temporary:
        stage = Path(temporary) / 'package'
        shutil.copytree(ROOT / 'extensions/railoxide', stage)
        for crate in ('browser_frontend', 'dapp_gateway_protocol'):
            compiled = wasm.with_name(f'{crate}.wasm')
            if not compiled.is_file():
                raise RuntimeError(f'Missing release WASM for {crate}. Run scripts/build-browser-extension.')
            subprocess.run([
                'wasm-bindgen', '--target', 'web', '--no-typescript',
                '--out-dir', str(stage), '--out-name', crate, str(compiled),
            ], check=True)
        package_protocol_licenses(metadata, stage)
        wallet_assets = stage / 'assets/railgun-ui'
        shutil.copytree(ROOT / 'crates/railgun-ui/assets', wallet_assets)
        # Keep the source attribution beside the unchanged chain and token bytes.
        # Browser asset keys use forward slashes on every build host.
        asset_paths = sorted(path.relative_to(stage / 'assets').as_posix() for path in wallet_assets.rglob('*')
                             if path.is_file() and path.suffix in ('.svg', '.png'))
        shared_icons = stage / 'assets/ui/icons'
        shared_icons.mkdir(parents=True)
        for name in ('shield', 'arrow-big-right-dash', 'book-user', 'wallet', 'pencil', 'refresh-ccw',
                     'screen-share', 'monitor', 'qr-code', 'shield-keyhole', 'eye',
                     'tor-status', 'arrow-right-left', 'arrow-down-to-line'):
            path = shared_icons / f'{name}.svg'
            shutil.copy2(ROOT / 'crates/ui/assets/icons' / path.name, path)
            asset_paths.append(path.relative_to(stage / 'assets').as_posix())
        wallet_icons = stage / 'assets/railgun/icons'
        wallet_icons.mkdir(parents=True)
        for name in ('clock.svg', 'dices.svg', 'ledger-logo-short-white.svg', 'trezor-symbol-white-rgb.svg'):
            path = wallet_icons / name
            shutil.copy2(ROOT / 'bins/wallet/assets/icons' / name, path)
            asset_paths.append(path.relative_to(stage / 'assets').as_posix())
        (stage / 'assets/WALLET-ASSETS.json').write_text(json.dumps(asset_paths) + '\n', encoding='utf-8')
        icons = package_source / 'assets/icons'
        target_icons = stage / 'assets/icons'
        shutil.copytree(icons, target_icons)
        # Preserve a deterministic mapping to locked source bytes, without local paths.
        mapping = [
            '# Packaged component assets', '',
            'Source: crates.io gpui-kit-assets 0.6.0, selected from locked Cargo metadata.',
            'Source assets/icons/*.svg maps to package assets/icons/*.svg unchanged.',
            'Kit package license: Apache-2.0; Lucide icons: ISC. See package licenses/.',
            '',
        ]
        for icon in sorted(target_icons.glob('*.svg')):
            digest = hashlib.sha256(icon.read_bytes()).hexdigest()
            mapping.append(f'- `assets/icons/{icon.name}` SHA-256 `{digest}`')
        (stage / 'assets/COMPONENT-SOURCES.md').write_text('\n'.join(mapping) + '\n', encoding='utf-8')
        shutil.copy2(ROOT / 'bins/wallet/assets/icons/SOURCES.md', stage / 'licenses/wallet-icon-SOURCES.md')
        shutil.copy2(ROOT / 'bins/wallet/assets/icons/lucide-icons-LICENSE.txt', stage / 'licenses/lucide-icons-LICENSE.txt')
        if output.is_symlink():
            raise RuntimeError('Refusing to replace a symlink at target/browser-extension')
        if output.exists():
            shutil.rmtree(output)
        shutil.move(str(stage), output)
    files = [p for p in output.rglob('*') if p.is_file()]
    archive = write_archive(output, files)
    print(f'Load unpacked: {output}')
    print(f'Archive: {archive}')
    print('Rebuild the desktop app to embed the archive. If it was built before the archive existed, touch bins/wallet/build.rs first.')
    print(f'Package: {len(files)} files, {sum(p.stat().st_size for p in files)} bytes')


if __name__ == '__main__':
    if len(sys.argv) != 2:
        raise SystemExit('This helper expects the locked Cargo metadata file from build-browser-extension.')
    package(sys.argv[1])
