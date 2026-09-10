#!/usr/bin/env python3
"""Stage locked browser assets and web bindings. Called by build-browser-extension."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

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
    (notices / 'SOURCES.md').write_text('\n'.join(index) + '\n')


def package(metadata_path):
    metadata = json.loads(Path(metadata_path).read_text())
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
        (stage / 'assets/COMPONENT-SOURCES.md').write_text('\n'.join(mapping) + '\n')
        shutil.copy2(ROOT / 'bins/wallet/assets/icons/SOURCES.md', stage / 'licenses/wallet-icon-SOURCES.md')
        if output.is_symlink():
            raise RuntimeError('Refusing to replace a symlink at target/browser-extension')
        if output.exists():
            shutil.rmtree(output)
        shutil.move(str(stage), output)
    files = [p for p in output.rglob('*') if p.is_file()]
    print(f'Load unpacked: {output}')
    print(f'Package: {len(files)} files, {sum(p.stat().st_size for p in files)} bytes')


if __name__ == '__main__':
    if len(sys.argv) != 2:
        raise SystemExit('This helper expects the locked Cargo metadata file from build-browser-extension.')
    package(sys.argv[1])
