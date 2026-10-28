#!/usr/bin/env python3
"""macOS release packaging helpers: wallet version, cask rendering and linkage checks."""

import argparse
import os
from pathlib import Path
import re
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / 'bins/wallet/Cargo.toml'
CASK_TEMPLATE = ROOT / 'bins/wallet/packaging/macos/railoxide.rb.in'
VERSION_PATTERN = re.compile(r'(\d+)\.(\d+)\.(\d+)(?:[-+][0-9A-Za-z.+-]+)?')
DIGEST_PATTERN = re.compile(r'[0-9a-f]{64}')
CHECKSUM_LINE = re.compile(r'(\S+) [ *](.+)')
MACHO_MAGICS = {
    bytes.fromhex(magic)
    for magic in ('feedface', 'feedfacf', 'cefaedfe', 'cffaedfe', 'cafebabe', 'cafebabf')
}
ALLOWED_PREFIXES = ('/usr/lib/', '/System/Library/')


class PackagingError(Exception):
    pass


def checked_version(version):
    match = VERSION_PATTERN.fullmatch(version)
    if match is None:
        raise PackagingError(f'unsupported wallet version: {version!r}')
    return match


def manifest_version(path):
    # Line scan, not tomllib: source installs may run Apple's Python 3.9.
    try:
        with open(path, encoding='utf-8') as manifest:
            lines = manifest.read().splitlines()
    except OSError as error:
        raise PackagingError(f'cannot read [package].version from {path}: {error}') from error
    in_package = False
    for line in lines:
        stripped = line.strip()
        if stripped == '[package]':
            in_package = True
            continue
        if in_package and stripped.startswith('['):
            break
        if in_package:
            match = re.fullmatch(r'version\s*=\s*"([^"]+)"', stripped)
            if match is not None:
                return match.group(1)
    raise PackagingError(f'cannot read [package].version from {path}: not found in [package]')


def command_version(args):
    match = checked_version(manifest_version(args.manifest))
    print('.'.join(match.groups()) if args.bundle else match.group(0))


def dmg_digest(checksums, dmg_name):
    try:
        lines = Path(checksums).read_text().splitlines()
    except OSError as error:
        raise PackagingError(f'cannot read checksum file: {error}') from error
    digests = []
    for line in filter(str.strip, lines):
        parsed = CHECKSUM_LINE.fullmatch(line)
        if parsed is None:
            raise PackagingError(f'malformed checksum line: {line!r}')
        if parsed.group(2) == dmg_name:
            digests.append(parsed.group(1))
    if len(digests) != 1:
        raise PackagingError(f'expected one checksum for {dmg_name}, found {len(digests)}')
    if DIGEST_PATTERN.fullmatch(digests[0]) is None:
        raise PackagingError(f'malformed SHA-256 digest for {dmg_name}: {digests[0]!r}')
    return digests[0]


def command_render_cask(args):
    version = checked_version(args.version).group(0)
    digest = dmg_digest(args.checksums, f'RailOxide-v{version}-macos-arm64.dmg')
    cask = CASK_TEMPLATE.read_text().replace('@VERSION@', version).replace('@SHA256@', digest)
    leftover = re.search(r'@[A-Za-z0-9_]+@', cask)
    if leftover is not None:
        raise PackagingError(f'unrendered cask placeholder: {leftover.group(0)}')
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(cask)


def is_macho(path):
    try:
        with open(path, 'rb') as candidate:
            return candidate.read(4) in MACHO_MAGICS
    except OSError as error:
        raise PackagingError(f'cannot read {path}: {error}') from error


def macho_files(paths):
    for root in paths:
        if os.path.isfile(root) and not os.path.islink(root):
            if is_macho(root):
                yield root
        elif os.path.isdir(root) and not os.path.islink(root):
            for directory, _, names in os.walk(root):
                for name in sorted(names):
                    path = os.path.join(directory, name)
                    if not os.path.islink(path) and os.path.isfile(path) and is_macho(path):
                        yield path
        else:
            raise PackagingError(f'not a regular file or directory: {root}')


def otool(flag, path):
    try:
        result = subprocess.run(['otool', flag, path], capture_output=True, text=True)
    except FileNotFoundError as error:
        raise PackagingError('otool is required for linkage checks') from error
    if result.returncode != 0:
        raise PackagingError(f'otool {flag} failed for {path}: {result.stderr.strip()}')
    return result.stdout.splitlines()


def references(path):
    # otool prints a "<file>:" or "<file> (architecture <arch>):" header before each slice.
    own_ids = {line.strip() for line in otool('-D', path) if not line.rstrip().endswith(':')}
    for line in otool('-L', path):
        if line.rstrip().endswith(':') or not line.strip():
            continue
        library = re.sub(r' \(compatibility version [^)]*\)$', '', line.strip())
        if library not in own_ids:
            yield library
    in_rpath = False
    for line in otool('-l', path):
        fields = line.split()
        if fields[:2] == ['cmd', 'LC_RPATH']:
            in_rpath = True
        elif in_rpath and fields[:1] == ['path']:
            yield re.sub(r' \(offset \d+\)$', '', line.strip()[len('path '):])
            in_rpath = False


def command_check_linkage(args):
    files = list(macho_files(args.paths))
    if not files:
        raise PackagingError('no Mach-O files found')
    offending = [
        (path, reference)
        for path in files
        for reference in references(path)
        if not reference.startswith(ALLOWED_PREFIXES)
    ]
    for path, reference in offending:
        print(f'{path}: {reference}', file=sys.stderr)
    if offending:
        raise PackagingError(f'{len(offending)} disallowed linkage reference(s)')
    print(f'linkage ok: {len(files)} Mach-O file(s) reference only system libraries')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)

    version = commands.add_parser('version', help='print the wallet version')
    version.add_argument('--manifest', default=MANIFEST)
    version.add_argument('--bundle', action='store_true', help='print MAJOR.MINOR.PATCH only')
    version.set_defaults(handler=command_version)

    cask = commands.add_parser('render-cask', help='render the Homebrew cask')
    cask.add_argument('--version', required=True)
    cask.add_argument('--checksums', required=True)
    cask.add_argument('--output', required=True)
    cask.set_defaults(handler=command_render_cask)

    linkage = commands.add_parser('check-linkage', help='reject non-system Mach-O references')
    linkage.add_argument('paths', nargs='+')
    linkage.set_defaults(handler=command_check_linkage)

    args = parser.parse_args()
    try:
        args.handler(args)
    except PackagingError as error:
        print(f'error: {error}', file=sys.stderr)
        sys.exit(1)


if __name__ == '__main__':
    main()
