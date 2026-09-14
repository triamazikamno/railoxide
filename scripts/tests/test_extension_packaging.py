"""Regression checks for extension build ordering, embedding, and helper integrity."""

import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path, PureWindowsPath
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile


ROOT = Path(__file__).resolve().parents[2]


def load_script(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'scripts' / f'{name}.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def write_zip(path, content=b'first'):
    with zipfile.ZipFile(path, 'w') as archive:
        archive.writestr('manifest.json', '{"version":"1.0.0"}')
        for name in ('browser_frontend_bg.wasm', 'dapp_gateway_protocol_bg.wasm'):
            archive.writestr(name, b'\0asm' + content)


class ExtensionPackagingTests(unittest.TestCase):
    def test_packager_preserves_utf8_and_browser_paths_on_windows(self):
        packager = load_script('package-browser-extension')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for directory in ('extensions/railoxide', 'crates/railgun-ui/assets',
                              'crates/ui/assets/icons', 'bins/wallet/assets/icons',
                              'bins/wallet/packaging/icons/png'):
                shutil.copytree(ROOT / directory, root / directory)
            shutil.copy2(ROOT / 'LICENSE', root / 'LICENSE')
            kit = root / 'kit-\u0141'
            (kit / 'assets/icons').mkdir(parents=True)
            icon_name = '\u0141.svg'
            icon = b'<svg/>'
            (kit / 'assets/icons' / icon_name).write_bytes(icon)
            wasm_directory = root / 'target/wasm32-unknown-unknown/release'
            wasm_directory.mkdir(parents=True)
            for crate in ('browser_frontend', 'dapp_gateway_protocol'):
                (wasm_directory / f'{crate}.wasm').write_bytes(b'\0asm\1\0\0\0')
            metadata = {
                'packages': [
                    {'id': 'kit', 'name': 'gpui-kit-assets', 'version': '0.6.0',
                     'manifest_path': str(kit / 'Cargo.toml')},
                    {'id': 'protocol', 'name': 'dapp-gateway-protocol', 'version': '0.1.0',
                     'manifest_path': str(root / 'Cargo.toml'), 'license': 'MIT'},
                ],
                'resolve': {'nodes': [{'id': 'protocol', 'dependencies': []}]},
                'workspace_members': ['protocol'],
                'target_directory': str(root / 'target'),
            }
            metadata_path = root / 'metadata.json'
            metadata_path.write_text(json.dumps(metadata, ensure_ascii=False), encoding='utf-8')

            def bindgen(args, **kwargs):
                stage = Path(args[args.index('--out-dir') + 1])
                name = args[args.index('--out-name') + 1]
                shutil.copy2(args[-1], stage / f'{name}_bg.wasm')

            path_open = Path.open
            path_relative_to = Path.relative_to

            def windows_open(path, mode='r', buffering=-1, encoding=None, errors=None, newline=None):
                if 'b' not in mode and encoding in (None, 'locale'):
                    encoding = 'cp1252'
                return path_open(path, mode, buffering, encoding, errors, newline)

            def windows_relative_to(path, *args, **kwargs):
                return PureWindowsPath(path_relative_to(path, *args, **kwargs))

            with patch.object(packager, 'ROOT', root), patch.object(packager.subprocess, 'run', side_effect=bindgen), patch.object(Path, 'open', windows_open), patch.object(Path, 'relative_to', windows_relative_to):
                packager.package(metadata_path)

            with zipfile.ZipFile(root / 'target/browser-extension.zip') as archive:
                manifest = json.loads(archive.read('manifest.json'))
                for icons in (manifest['icons'], manifest['action']['default_icon']):
                    for path in icons.values():
                        source = ROOT / 'bins/wallet/packaging/icons/png' / f'logo-{Path(path).name}'
                        self.assertEqual(archive.read(path), source.read_bytes())
                self.assertEqual(archive.read(f'assets/icons/{icon_name}'), icon)
                self.assertIn(icon_name, archive.read('assets/COMPONENT-SOURCES.md').decode('utf-8'))
                wallet_assets = json.loads(archive.read('assets/WALLET-ASSETS.json'))
                self.assertIn('ui/icons/arrow-big-right-dash.svg', wallet_assets)
                for path in wallet_assets:
                    self.assertIn(f'assets/{path}', archive.namelist())

    def test_embedding_rejects_missing_and_stale_archives(self):
        verifier = load_script('verify-browser-extension')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / 'extension.zip'
            binary = root / 'wallet'
            write_zip(archive)
            binary.write_bytes(b'executable')
            with self.assertRaises(RuntimeError):
                verifier.verify(binary, archive)
            binary.write_bytes(b'executable' + archive.read_bytes())
            verifier.verify(binary, archive)
            write_zip(archive, b'updated')
            with self.assertRaises(RuntimeError):
                verifier.verify(binary, archive)

    def test_tool_install_checks_download_and_repairs_cache_without_extracting_links(self):
        installer = load_script('install-browser-extension-tools')
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = 'x86_64-unknown-linux-musl'
            name = f'wasm-bindgen-0.2.126-{target}'
            data = io.BytesIO()
            with tarfile.open(fileobj=data, mode='w:gz') as archive:
                for filename in ('wasm-bindgen', 'LICENSE-MIT', 'LICENSE-APACHE'):
                    entry = tarfile.TarInfo(f'{name}/{filename}')
                    entry.size = 4
                    archive.addfile(entry, io.BytesIO(b'tool'))
                link = tarfile.TarInfo(f'{name}/unused-link')
                link.type = tarfile.SYMTYPE
                link.linkname = '/outside'
                archive.addfile(link)
            archive_bytes = data.getvalue()
            pins = {'version': '0.2.126', 'targets': {target: hashlib.sha256(archive_bytes).hexdigest()}}
            (root / 'browser-extension-tools.json').write_text(json.dumps(pins))
            installer.__file__ = str(root / 'installer.py')
            destination = root / 'tools'
            payload = b'wrong download'

            def download(args, **kwargs):
                Path(args[args.index('--output') + 1]).write_bytes(payload)

            with patch.object(installer, 'host_target', return_value=target), patch.object(installer.subprocess, 'run', side_effect=download) as run:
                with self.assertRaisesRegex(RuntimeError, 'Checksum mismatch'):
                    installer.install(destination)
                self.assertFalse((destination / 'bin/wasm-bindgen').exists())
                payload = archive_bytes
                installer.install(destination)
                (destination / 'bin/wasm-bindgen').write_bytes(b'corrupt cache')
                installer.install(destination)
                self.assertEqual(run.call_count, 2)
            self.assertEqual((destination / 'bin/wasm-bindgen').read_bytes(), b'tool')
            self.assertFalse((destination / 'unused-link').is_symlink())


@unittest.skipIf(sys.platform == 'win32', 'Bash installer is tested on Linux and macOS')
class SourceBuildTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='extension build ')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / 'scripts').mkdir()
        (self.root / 'tools').mkdir()
        (self.root / 'target/release').mkdir(parents=True)
        self.env = dict(os.environ, PATH=f'{self.root}/tools:{os.environ["PATH"]}', TRACE=str(self.root / 'trace'))
        self.env.pop('RUSTC_BOOTSTRAP', None)
        self.env.pop('CARGO_TOOLCHAIN', None)
        self.env.pop('CARGO_BIN', None)
        self.env.pop('RAILOXIDE_EXTENSION_BUNDLE', None)
        self.write_tool('wasm-bindgen', "print('wasm-bindgen 0.2.126')")
        self.write_tool('cargo', '''
import json, os, pathlib, sys
args = sys.argv[1:]
with open(os.environ['TRACE'], 'a') as trace:
    trace.write(json.dumps([args, os.environ.get('RUSTC_BOOTSTRAP'), os.environ.get('RAILOXIDE_EXTENSION_BUNDLE')]) + '\\n')
if 'browser-frontend' in args and os.environ.get('FAIL_EXTENSION'):
    sys.exit(23)
if 'metadata' in args:
    print('{}')
if 'wallet' in args:
    bundle = os.environ.get('RAILOXIDE_EXTENSION_BUNDLE')
    pathlib.Path('target/release/wallet').write_bytes(b'executable' + (pathlib.Path(bundle).read_bytes() if bundle else b''))
''')
        shutil.copy2(ROOT / 'scripts/build-browser-extension', self.root / 'scripts')
        shutil.copy2(ROOT / 'scripts/verify-browser-extension.py', self.root / 'scripts')
        (self.root / 'scripts/package-browser-extension.py').write_text('''
import zipfile
with zipfile.ZipFile('target/browser-extension.zip', 'w') as archive:
    archive.writestr('manifest.json', '{"version":"1.0.0"}')
    for name in ('browser_frontend_bg.wasm', 'dapp_gateway_protocol_bg.wasm'):
        archive.writestr(name, b'\\0asmtest')
''')
        installer = (ROOT / 'scripts/install-wallet').read_text().rsplit('main "$@"', 1)[0]
        (self.root / 'installer-functions').write_text(installer)

    def write_tool(self, name, source):
        path = self.root / 'tools' / name
        path.write_text(f'#!{sys.executable}\n{source}\n')
        path.chmod(0o755)

    def run_installer_build(self):
        return subprocess.run(['bash', '-c', 'source ./installer-functions; source_dir="$PWD"; build_linux'],
                              cwd=self.root, env=self.env, capture_output=True, text=True)

    def trace(self):
        return [json.loads(line) for line in (self.root / 'trace').read_text().splitlines()]

    def test_installer_builds_and_embeds_before_wallet_without_bootstrap_leak(self):
        (self.root / 'target/release/wallet').write_bytes(b'previous wallet without extension')
        self.env['RAILOXIDE_EXTENSION_BUNDLE'] = str(self.root / 'stale-caller-path.zip')
        result = self.run_installer_build()
        self.assertEqual(result.returncode, 0, result.stderr)
        trace = self.trace()
        self.assertIn('browser-frontend', trace[0][0])
        self.assertEqual(trace[0][1], '1')
        self.assertIn('wallet', trace[-1][0])
        self.assertIsNone(trace[-1][1])
        # macOS temporary paths may use /var while Bash's PWD uses /private/var.
        self.assertEqual(Path(trace[-1][2]).resolve(), (self.root / 'target/browser-extension.zip').resolve())
        self.assertIn((self.root / 'target/browser-extension.zip').read_bytes(), (self.root / 'target/release/wallet').read_bytes())

    def test_extension_failure_stops_wallet_build_even_with_an_old_zip(self):
        write_zip(self.root / 'target/browser-extension.zip')
        self.env['FAIL_EXTENSION'] = '1'
        result = self.run_installer_build()
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertFalse(any('wallet' in item[0] for item in self.trace()))

    def test_legacy_source_builds_without_extension_tools(self):
        (self.root / 'scripts/build-browser-extension').unlink()
        (self.root / 'tools/wasm-bindgen').unlink()
        result = self.run_installer_build()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.trace()), 1)
        self.assertFalse((self.root / 'target/browser-extension.zip').exists())

    def test_macos_packager_embeds_extension_with_its_selected_cargo(self):
        shutil.copy2(ROOT / 'scripts/package-wallet-macos', self.root / 'scripts')
        for name in ('macos/Info.plist', 'icons/macos/RailOxide.icns'):
            path = self.root / 'bins/wallet/packaging' / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b'fixture')
        self.write_tool('uname', "print('Darwin')")
        for name in ('codesign', 'ditto', 'hdiutil', 'plutil'):
            self.write_tool(name, 'pass')
        env = dict(self.env, CARGO_BIN=str(self.root / 'tools/cargo'), CARGO_TOOLCHAIN='1.97.1')
        # Calling from another directory must still build this checkout.
        result = subprocess.run(['bash', str(self.root / 'scripts/package-wallet-macos')],
                                cwd=self.root.parent, env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        binary = self.root / 'target/macos/RailOxide.app/Contents/MacOS/RailOxide'
        self.assertIn((self.root / 'target/browser-extension.zip').read_bytes(), binary.read_bytes())
        self.assertTrue(all(entry[0][0] == '+1.97.1' for entry in self.trace()))
        self.assertIsNone(self.trace()[-1][1])

    def test_builder_supports_stable_explicit_and_standalone_cargo(self):
        for toolchain in (None, '1.97.1', ''):
            with self.subTest(toolchain=toolchain):
                env = dict(self.env, CARGO_BIN=str(self.root / 'tools/cargo'))
                if toolchain is not None:
                    env['CARGO_TOOLCHAIN'] = toolchain
                result = subprocess.run(['bash', 'scripts/build-browser-extension'], cwd=self.root, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                for entry in self.trace()[-2:]:
                    if toolchain == '':
                        self.assertFalse(entry[0][0].startswith('+'))
                    else:
                        self.assertEqual(entry[0][0], '+' + (toolchain or 'stable'))


if __name__ == '__main__':
    unittest.main()
