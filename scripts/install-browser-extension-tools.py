#!/usr/bin/env python3
"""Install the pinned wasm-bindgen helper into a private build-tools directory."""

import argparse
import hashlib
import json
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import tempfile


def host_target():
    system = platform.system()
    # The Windows installer builds x64, including under emulation on ARM64.
    if system == 'Windows':
        return 'x86_64-pc-windows-msvc'
    arch = {'arm64': 'aarch64', 'AMD64': 'x86_64'}.get(platform.machine(), platform.machine())
    suffix = {'Darwin': 'apple-darwin', 'Linux': 'unknown-linux-musl'}[system]
    return f'{arch}-{suffix}'


def install(destination):
    pins = json.loads(Path(__file__).with_name('browser-extension-tools.json').read_text())
    target = host_target()
    digest = pins['targets'][target]
    name = f"wasm-bindgen-{pins['version']}-{target}"
    destination = Path(destination).resolve()
    destination.mkdir(parents=True, exist_ok=True)
    archive = destination / f'{name}.tar.gz'
    with tempfile.TemporaryDirectory(prefix='wasm-bindgen-', dir=destination) as temporary:
        temporary = Path(temporary)
        if not archive.is_file() or hashlib.sha256(archive.read_bytes()).hexdigest() != digest:
            downloaded = temporary / 'download.tar.gz'
            url = f"https://github.com/wasm-bindgen/wasm-bindgen/releases/download/{pins['version']}/{name}.tar.gz"
            subprocess.run(['curl', '--fail', '--location', '--silent', '--show-error',
                            '--proto', '=https', '--proto-redir', '=https',
                            '--output', str(downloaded), url], check=True)
            if hashlib.sha256(downloaded.read_bytes()).hexdigest() != digest:
                raise RuntimeError(f'Checksum mismatch for {name}')
            downloaded.replace(archive)
        binary = 'wasm-bindgen.exe' if target.endswith('windows-msvc') else 'wasm-bindgen'
        # Copy only the expected regular files; never extract archive paths or links.
        with tarfile.open(archive) as bundle:
            for filename in (binary, 'LICENSE-MIT', 'LICENSE-APACHE'):
                member = bundle.getmember(f'{name}/{filename}')
                if not member.isfile():
                    raise RuntimeError(f'Expected a regular file: {member.name}')
                staged = temporary / filename
                with bundle.extractfile(member) as source, staged.open('wb') as output:
                    shutil.copyfileobj(source, output)
                directory = destination / ('bin' if filename == binary else 'licenses')
                directory.mkdir(exist_ok=True)
                staged.chmod(0o755 if filename == binary else 0o644)
                staged.replace(directory / filename)
    print(destination / 'bin')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('destination', help='private build-tools directory')
    install(parser.parse_args().destination)
