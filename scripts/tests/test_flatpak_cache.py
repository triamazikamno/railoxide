"""Protect Flatpak cache routing and build failure propagation."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
PACKAGING = ROOT / 'bins/wallet/packaging/flatpak'


class FlatpakCacheTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.events = self.root / 'events'
        self.env = {
            **os.environ,
            'PATH': f'{self.root}{os.pathsep}{os.environ["PATH"]}',
            'CACHE_EVENTS': str(self.events),
            'CARGO_HOME': str(self.root / 'cargo home'),
        }

    def executable(self, name, body):
        path = self.root / name
        path.write_text('#!/usr/bin/env python3\n' + body)
        path.chmod(0o755)
        return path

    def read_events(self):
        return [json.loads(line) for line in self.events.read_text().splitlines()]

    def test_only_vendored_dependencies_use_cache_and_compiler_status_is_preserved(self):
        compiler = self.executable('compiler', '''import json, os, sys
with open(os.environ['CACHE_EVENTS'], 'a') as output:
    output.write(json.dumps(['compiler', *sys.argv[1:]]) + '\\n')
sys.exit(23)
''')
        self.executable('sccache', '''import json, os, sys
with open(os.environ['CACHE_EVENTS'], 'a') as output:
    output.write(json.dumps(['cache']) + '\\n')
os.execv(sys.argv[1], sys.argv[1:])
''')
        arguments = ['--crate-name', 'dependency', 'source with spaces.rs']
        for manifest, cached in [
            (str(Path(self.env['CARGO_HOME']) / 'vendor/dependency'), True),
            (str(self.root / 'crates/workspace'), False),
            (str(Path(self.env['CARGO_HOME']) / 'vendor-other/dependency'), False),
        ]:
            with self.subTest(manifest=manifest):
                self.events.write_text('')
                result = subprocess.run(
                    [PACKAGING / 'rustc-cache', compiler, *arguments],
                    env={**self.env, 'CARGO_MANIFEST_DIR': manifest},
                    capture_output=True, text=True,
                )
                self.assertEqual(result.returncode, 23, result.stderr)
                expected = [['cache']] if cached else []
                self.assertEqual(self.read_events(), expected + [['compiler', *arguments]])

    def test_stats_and_shutdown_errors_do_not_change_build_result(self):
        self.executable('sccache', '''import json, os, sys
with open(os.environ['CACHE_EVENTS'], 'a') as output:
    output.write(json.dumps(sys.argv[1:]) + '\\n')
sys.exit(0 if sys.argv[1] == '--start-server' else 7)
''')
        for status in (0, 23):
            with self.subTest(status=status):
                self.events.write_text('')
                result = subprocess.run(
                    [PACKAGING / 'run-cached-build', 'sh', '-c', f'exit {status}'],
                    env=self.env, capture_output=True, text=True,
                )
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertEqual(self.read_events(), [
                    ['--start-server'], ['--show-stats'], ['--stop-server'],
                ])


if __name__ == '__main__':
    unittest.main()
