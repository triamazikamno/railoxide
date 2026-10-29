"""Protect macOS release versioning and the generated Homebrew cask."""

import hashlib
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / 'scripts/macos-packaging.py'
VERSION = '0.2.0-beta.10'
DMG = f'RailOxide-v{VERSION}-macos-arm64.dmg'


class MacosPackagingTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def helper(self, *arguments):
        return subprocess.run(
            [sys.executable, HELPER, *arguments], capture_output=True, text=True,
        )

    def test_version_maps_to_bundle_version(self):
        manifest = self.root / 'Cargo.toml'
        for version, bundle in [(VERSION, '0.2.0'), ('1.2.3', '1.2.3'), ('1.2', None)]:
            with self.subTest(version=version):
                manifest.write_text(f'[package]\nname = "wallet"\nversion = "{version}"\n')
                full = self.helper('version', '--manifest', manifest)
                short = self.helper('version', '--manifest', manifest, '--bundle')
                if bundle is None:
                    self.assertNotEqual(full.returncode, 0)
                    self.assertNotEqual(short.returncode, 0)
                    continue
                self.assertEqual(full.returncode, 0, full.stderr)
                self.assertEqual(full.stdout.strip(), version)
                self.assertEqual(short.returncode, 0, short.stderr)
                self.assertEqual(short.stdout.strip(), bundle)

    def render(self, checksums):
        path = self.root / 'SHA256SUMS'
        path.write_text(checksums)
        output = self.root / 'out/railoxide.rb'
        result = self.helper(
            'render-cask', '--version', VERSION, '--checksums', path, '--output', output,
        )
        return result, output

    def test_cask_pins_release_asset_and_scopes_quarantine_removal(self):
        digest = hashlib.sha256(b'dmg').hexdigest()
        result, output = self.render(f'{digest}  {DMG}\n')
        self.assertEqual(result.returncode, 0, result.stderr)
        cask = output.read_text()
        self.assertIn(
            '  url "https://github.com/triamazikamno/railoxide/releases/download/'
            'v#{version}/RailOxide-v#{version}-macos-arm64.dmg"\n', cask,
        )
        self.assertNotIn('latest', cask)
        self.assertIn(f'version "{VERSION}"', cask)
        self.assertIn(f'sha256 "{digest}"', cask)
        self.assertIn('depends_on arch: :arm64', cask)
        self.assertIn('depends_on macos: :sequoia', cask)
        self.assertIn('app "RailOxide.app"', cask)
        hooks = re.findall(
            r'^\s*(zap|uninstall\w*|preflight\w*|postflight\w*|auto_updates|installer)\b',
            cask, re.MULTILINE,
        )
        self.assertEqual(hooks, ['postflight_steps'])
        postflight = cask.split('postflight_steps do', 1)[1].split('\n  end', 1)[0]
        self.assertEqual(re.findall(r'"([^"]*/[^"]*)"', postflight), [
            '/usr/bin/find', '{{appdir}}/RailOxide.app', '/usr/bin/xattr', '{{appdir}}/RailOxide.app',
        ])

    def test_cask_requires_a_valid_dmg_checksum(self):
        digest = hashlib.sha256(b'dmg').hexdigest()
        for checksums in [
            f'{digest}  RailOxide-v{VERSION}-macos-SHA256SUMS\n',
            f'{digest[:-1]}  {DMG}\n',
            f'{digest.upper()}  {DMG}\n',
        ]:
            with self.subTest(checksums=checksums):
                result, output = self.render(checksums)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(output.exists())

    def test_packager_parses(self):
        result = subprocess.run(
            ['bash', '-n', ROOT / 'scripts/package-wallet-macos'], capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == '__main__':
    unittest.main()
