"""Protect the generated Scoop manifest and the Windows DLL import check."""

import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / 'scripts/windows-packaging.py'
VERSION = '0.2.0-beta.10'
ZIP = f'RailOxide-v{VERSION}-windows-x64.zip'
DUMPBIN_OUTPUT = """\
Dump of file wallet.exe

File Type: EXECUTABLE IMAGE

  Image has the following dependencies:

    KERNEL32.dll
    api-ms-win-core-winrt-error-l1-1-0.dll

  Image has the following delay load dependencies:

    API-MS-WIN-CORE-UNLISTED-L1-1-0.dll

  Summary

        1000 .data
"""


def load_helper():
    spec = importlib.util.spec_from_file_location('windows_packaging', HELPER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class WindowsPackagingTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def render(self, checksums):
        path = self.root / 'SHA256SUMS'
        path.write_text(checksums)
        output = self.root / 'out/railoxide.json'
        result = subprocess.run(
            [sys.executable, HELPER, 'render-manifest',
             '--version', VERSION, '--checksums', path, '--output', output],
            capture_output=True, text=True,
        )
        return result, output

    def test_manifest_pins_release_asset_and_runs_no_scripts(self):
        digest = hashlib.sha256(b'zip').hexdigest()
        result, output = self.render(f'{digest}  RailOxide-v{VERSION}-macos-arm64.dmg\n{digest[::-1]}  {ZIP}\n')
        self.assertEqual(result.returncode, 0, result.stderr)
        manifest = json.loads(output.read_text())
        self.assertEqual(manifest['version'], VERSION)
        self.assertEqual(manifest['architecture'], {'64bit': {
            'url': f'https://github.com/triamazikamno/railoxide/releases/download/v{VERSION}/{ZIP}',
            'hash': digest[::-1],
            'extract_dir': f'RailOxide-v{VERSION}-windows-x64',
        }})
        self.assertEqual(manifest['shortcuts'], [['wallet.exe', 'RailOxide', '', 'RailOxide.ico']])
        self.assertIn('not code-signed', manifest['notes'])
        for field in ('bin', 'persist', 'pre_install', 'post_install', 'installer', 'uninstaller',
                      'checkver', 'autoupdate', 'url', 'hash'):
            self.assertNotIn(field, manifest)

    def test_manifest_requires_the_zip_checksum(self):
        digest = hashlib.sha256(b'zip').hexdigest()
        result, output = self.render(f'{digest}  RailOxide-v{VERSION}-windows-SHA256SUMS\n')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(output.exists())

    def test_import_check_reads_both_sections_and_rejects_non_system_dlls(self):
        helper = load_helper()
        names = helper.parse_dependents(DUMPBIN_OUTPUT)
        self.assertEqual(names, [
            'KERNEL32.dll', 'api-ms-win-core-winrt-error-l1-1-0.dll', 'API-MS-WIN-CORE-UNLISTED-L1-1-0.dll',
        ])
        self.assertEqual(helper.disallowed_imports(names), ['API-MS-WIN-CORE-UNLISTED-L1-1-0.dll'])
        rejected = ['VCRUNTIME140.dll', 'api-ms-win-crt-runtime-l1-1-0.dll', 'sqlite3.dll']
        self.assertEqual(helper.disallowed_imports(rejected), rejected)
        allowlist = helper.SYSTEM_DLLS | {'vcomp140.dll'}
        self.assertEqual(helper.disallowed_imports(['VCOMP140.dll'], allowlist), ['VCOMP140.dll'])


if __name__ == '__main__':
    unittest.main()
