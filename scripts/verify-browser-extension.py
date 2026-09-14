#!/usr/bin/env python3
"""Check that a packaged wallet contains the complete extension archive."""

import argparse
import json
import mmap
from pathlib import Path
import zipfile


def verify(binary, archive):
    archive = Path(archive)
    with zipfile.ZipFile(archive) as bundle:
        if bundle.testzip() is not None:
            raise RuntimeError('Extension ZIP integrity check failed')
        manifest = json.loads(bundle.read('manifest.json'))
        if not manifest.get('version'):
            raise RuntimeError('Extension manifest has no version')
        for name in ('browser_frontend_bg.wasm', 'dapp_gateway_protocol_bg.wasm'):
            if bundle.read(name)[:4] != b'\0asm':
                raise RuntimeError(f'Missing WASM module: {name}')
    with Path(binary).open('rb') as wallet, mmap.mmap(wallet.fileno(), 0, access=mmap.ACCESS_READ) as data:
        if data.find(archive.read_bytes()) == -1:
            raise RuntimeError(f'{binary} does not contain {archive}; rebuild the wallet with RAILOXIDE_EXTENSION_BUNDLE set')
    print(f'Verified embedded extension {manifest["version"]}: {binary}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary')
    parser.add_argument('archive')
    args = parser.parse_args()
    verify(args.binary, args.archive)
