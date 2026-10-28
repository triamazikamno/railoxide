"""Protect release source checks and per-platform GitHub Release asset ownership."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / 'scripts/release-assets'
REPO = 'owner/repo'
TAG = 'v1.2.3'

FAKE_GH = r'''import json, os, shutil, sys
state_path = os.environ['FAKE_GH_STATE']
with open(os.environ['FAKE_GH_LOG'], 'a') as log:
    log.write(json.dumps(sys.argv[1:]) + '\n')
with open(state_path) as source:
    state = json.load(source)
args = sys.argv[1:]
if args[:1] == ['api']:
    if args[1] != f"repos/{state['repo']}/releases/tags/{state['tag']}":
        sys.exit(1)
    print(json.dumps({'tag_name': state['tag'], 'assets': [
        {'name': a['name'], 'state': a['state'], 'browser_download_url': 'file://' + a['path']}
        for a in state['assets']
    ]}))
elif args[:2] == ['release', 'upload']:
    files = [arg for arg in args[2:] if not arg.startswith('--')]
    tag, repo, files = files[0], files[1], files[2:]
    if tag != state['tag'] or repo != state['repo']:
        sys.exit(1)
    names = {a['name'] for a in state['assets']}
    if any(os.path.basename(f) in names for f in files):
        sys.exit(1)
    for path in files:
        name = os.path.basename(path)
        if name == os.environ.get('FAKE_GH_DROP'):
            continue
        stored = os.path.join(state['storage'], f"{len(state['assets'])}-{name}")
        shutil.copyfile(path, stored)
        state['assets'].append({'name': name, 'state': 'uploaded', 'path': stored})
    with open(state_path, 'w') as output:
        json.dump(state, output)
else:
    sys.exit(2)
'''


def run(*args, cwd, env=None, check=True):
    result = subprocess.run(args, cwd=cwd, env=env, capture_output=True, text=True)
    if check and result.returncode != 0:
        raise AssertionError(f'{args} failed: {result.stderr}')
    return result


class Keys:
    """Two throwaway signing keys; only `release` is trusted by the publisher."""

    def __init__(self, root):
        self.homes = {}
        for label in ('release', 'other'):
            home = root / f'gpg-{label}'
            home.mkdir(mode=0o700)
            run('gpg', '--batch', '--homedir', home, '--passphrase', '', '--quick-gen-key',
                f'{label} <{label}@example.invalid>', 'ed25519', 'sign', 'never', cwd=root)
            self.homes[label] = home
        self.public = root / 'release-public.gpg'
        run('gpg', '--batch', '--homedir', self.homes['release'], '--export', '-o', self.public, cwd=root)

    def sign(self, data, output, key='release', *extra):
        output.unlink(missing_ok=True)
        run('gpg', '--batch', '--homedir', self.homes[key], '--pinentry-mode', 'loopback',
            '--passphrase', '', *extra, '--armor', '--detach-sign', '-o', output, data, cwd=data.parent)
        return output

    def close(self):
        for home in self.homes.values():
            subprocess.run(['gpgconf', '--homedir', home, '--kill', 'all'], capture_output=True)


class ReleaseAssetsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        temporary = tempfile.TemporaryDirectory()
        cls.addClassCleanup(temporary.cleanup)
        cls.keys = Keys(Path(temporary.name))
        cls.addClassCleanup(cls.keys.close)

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.storage = self.root / 'storage'
        self.storage.mkdir()
        self.state = self.root / 'state.json'
        self.log = self.root / 'gh.log'
        self.log.write_text('')
        self.save({'repo': REPO, 'tag': TAG, 'storage': str(self.storage), 'assets': []})
        bin_dir = self.root / 'bin'
        bin_dir.mkdir()
        (bin_dir / 'gh').write_text('#!/usr/bin/env python3\n' + FAKE_GH)
        (bin_dir / 'gh').chmod(0o755)
        self.env = {
            **os.environ,
            'PATH': f'{bin_dir}{os.pathsep}{os.environ["PATH"]}',
            'FAKE_GH_STATE': str(self.state),
            'FAKE_GH_LOG': str(self.log),
            'GIT_CONFIG_GLOBAL': os.devnull,
            'GIT_CONFIG_NOSYSTEM': '1',
            'GIT_AUTHOR_NAME': 'Test', 'GIT_AUTHOR_EMAIL': 'test@example.invalid',
            'GIT_COMMITTER_NAME': 'Test', 'GIT_COMMITTER_EMAIL': 'test@example.invalid',
        }
        self.env.pop('FAKE_GH_DROP', None)
        self.repo = self.root / 'work'
        run('git', 'init', '-q', '--bare', '-b', 'main', self.root / 'origin.git', cwd=self.root, env=self.env)
        run('git', 'init', '-q', '-b', 'main', self.repo, cwd=self.root, env=self.env)
        self.git('remote', 'add', 'origin', str(self.root / 'origin.git'))
        self.release_sha = self.commit({
            'Cargo.toml': '[workspace]\nmembers = ["bins/wallet", "crates/lib"]\n',
            'bins/wallet/Cargo.toml': '[package]\nname = "wallet"\nversion = "1.2.3"\n\n'
                                      '[dependencies]\nlib = { path = "../../crates/lib" }\n',
            'crates/lib/Cargo.toml': '[package]\nname = "lib"\nversion = "0.1.0"\n',
        }, main=True, tag=TAG)
        self.dist = self.root / 'dist'
        self.dist.mkdir()

    def save(self, state):
        self.state.write_text(json.dumps(state))

    def git(self, *args):
        return run('git', *args, cwd=self.repo, env=self.env).stdout.strip()

    def commit(self, files, main=False, tag=None):
        for name, content in files.items():
            path = self.repo / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
        self.git('add', '-A')
        self.git('commit', '-q', '-m', 'commit')
        sha = self.git('rev-parse', 'HEAD')
        if main:
            self.git('push', '-q', 'origin', f'{sha}:refs/heads/main')
        if tag:
            self.git('push', '-q', 'origin', f'{sha}:refs/tags/{tag}')
        return sha

    def file(self, name, data, directory=None):
        path = (directory or self.dist) / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return path

    def macos_files(self):
        dmg = self.file(f'RailOxide-{TAG}-macos-arm64.dmg', b'dmg bytes')
        sums = self.file(f'RailOxide-{TAG}-macos-SHA256SUMS', b'0123  RailOxide.dmg\n')
        return [dmg, sums, self.keys.sign(sums, self.dist / f'{sums.name}.asc')]

    def flatpak_files(self):
        directory = self.root / 'flatpak'
        bundle = self.file(f'RailOxide-{TAG}-x86_64.flatpak', b'flatpak bundle', directory)
        sums = self.file('SHA256SUMS', b'4567  bundle\n', directory)
        return [bundle, sums, self.keys.sign(sums, directory / 'SHA256SUMS.asc')]

    def seed(self, path, data=None, state='uploaded', name=None):
        current = json.loads(self.state.read_text())
        stored = self.storage / f'{len(current["assets"])}-seed'
        stored.write_bytes(path.read_bytes() if data is None else data)
        current['assets'].append({'name': name or path.name, 'state': state, 'path': str(stored)})
        self.save(current)

    def published(self):
        return {asset['name']: Path(asset['path']).read_bytes()
                for asset in json.loads(self.state.read_text())['assets']}

    def gh_calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def uploads(self):
        return [call for call in self.gh_calls() if call[:2] == ['release', 'upload']]

    def publish(self, files, existing='verify', source=None, env=None):
        args = [SCRIPT, 'publish', '--repo', REPO, '--tag', TAG, '--existing', existing,
                '--public-key', self.keys.public]
        if source:
            args += ['--source-commit', source]
        return run(*args, *files, cwd=self.repo, env=env or self.env, check=False)

    def assert_failed(self, result):
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn('error: ', result.stderr)

    def test_accepted_source_publishes_macos_beside_flatpak(self):
        result = run(SCRIPT, 'check-source', '--tag', TAG, '--commit', self.release_sha,
                     '--require-head', cwd=self.repo, env=self.env, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f'release source verified: {TAG} {self.release_sha}\n')

        for path in self.flatpak_files():
            self.seed(path)
        before = self.published()
        files = self.macos_files()
        result = self.publish(files, source=self.release_sha)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.uploads()), 1)
        upload = self.uploads()[0]
        self.assertNotIn('--clobber', upload)
        self.assertEqual(sorted(Path(arg).name for arg in upload[5:]), sorted(f.name for f in files))
        after = self.published()
        self.assertEqual({name: after[name] for name in before}, before)
        self.assertEqual({f.name: after[f.name] for f in files}, {f.name: f.read_bytes() for f in files})

    def test_rejected_sources_stop_before_github(self):
        cases = [
            ('tag points elsewhere', TAG, self.commit({'README': 'later\n'}, main=True)),
            ('version differs from tag', 'v1.2.4', self.commit({'README': 'x\n'}, main=True, tag='v1.2.4')),
            ('unresolved local patch', 'v1.2.3-patch', self.commit({
                'bins/wallet/Cargo.toml': '[package]\nname = "wallet"\nversion = "1.2.3-patch"\n',
                'Cargo.toml': '[workspace]\nmembers = ["bins/wallet"]\n\n'
                              '[patch."https://github.com/x/y"]\nfoo = { path = "../outside/foo" }\n',
            }, main=True, tag='v1.2.3-patch')),
            # Committed on top of main without pushing main.
            ('not on main', 'v1.2.3-side',
             self.commit({'bins/wallet/Cargo.toml': '[package]\nname = "wallet"\nversion = "1.2.3-side"\n'},
                         tag='v1.2.3-side')),
        ]
        files = self.macos_files()
        for label, tag, sha in cases:
            with self.subTest(label):
                self.log.write_text('')
                result = run(SCRIPT, 'publish', '--repo', REPO, '--tag', tag, '--existing', 'verify',
                             '--public-key', self.keys.public, '--source-commit', sha, *files,
                             cwd=self.repo, env=self.env, check=False)
                self.assert_failed(result)
                self.assertEqual(self.gh_calls(), [])

    def test_retry_accepts_matching_assets(self):
        files = self.macos_files()
        dmg, sums, signature = files
        # ed25519 signatures are deterministic; a notation keeps same-second re-signatures distinct.
        resigned = self.keys.sign(sums, self.root / signature.name, 'release',
                                  '--sig-notation', 'retry@railoxide.invalid=1')
        self.assertNotEqual(resigned.read_bytes(), signature.read_bytes())
        for label, seeded, uploaded in [
            ('complete', [dmg, sums, resigned], []),
            ('partial', [dmg], [sums.name, signature.name]),
        ]:
            with self.subTest(label):
                self.save({'repo': REPO, 'tag': TAG, 'storage': str(self.storage), 'assets': []})
                self.log.write_text('')
                for path in seeded:
                    self.seed(path)
                result = self.publish(files)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual([[Path(arg).name for arg in call[5:]] for call in self.uploads()],
                                 [uploaded] if uploaded else [])

    def test_conflicting_owned_assets_fail_without_upload(self):
        files = self.macos_files()
        dmg, sums, signature = files
        foreign = self.keys.sign(sums, self.root / signature.name, 'other')
        for label, seed in [
            ('different dmg bytes', lambda: self.seed(dmg, b'other dmg')),
            ('signature by another key', lambda: self.seed(foreign)),
            ('incomplete upload', lambda: self.seed(sums, state='starter')),
        ]:
            with self.subTest(label):
                self.save({'repo': REPO, 'tag': TAG, 'storage': str(self.storage), 'assets': []})
                self.log.write_text('')
                seed()
                before = self.published()
                self.assert_failed(self.publish(files))
                self.assertEqual(self.uploads(), [])
                self.assertEqual(self.published(), before)

    def test_flatpak_rejects_only_its_own_names(self):
        macos = self.macos_files()
        flatpak = self.flatpak_files()
        for path in macos:
            self.seed(path)
        macos_bytes = self.published()

        self.seed(flatpak[0])
        self.assert_failed(self.publish(flatpak, existing='reject'))
        self.assertEqual(self.uploads(), [])
        self.assertEqual({name: self.published()[name] for name in macos_bytes}, macos_bytes)

        self.save({'repo': REPO, 'tag': TAG, 'storage': str(self.storage), 'assets': []})
        for path in macos:
            self.seed(path)
        result = self.publish(flatpak, existing='reject')
        self.assertEqual(result.returncode, 0, result.stderr)
        published = self.published()
        self.assertEqual({name: published[name] for name in macos_bytes}, macos_bytes)
        self.assertEqual({f.name: published[f.name] for f in flatpak}, {f.name: f.read_bytes() for f in flatpak})

    def test_owned_asset_missing_after_upload_fails(self):
        files = self.macos_files()
        self.assert_failed(self.publish(files, env={**self.env, 'FAKE_GH_DROP': files[1].name}))
        self.assertEqual(len(self.uploads()), 1)


if __name__ == '__main__':
    unittest.main()
