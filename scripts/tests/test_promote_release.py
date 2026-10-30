"""Protect release promotion: only the verified cask and manifest of the latest release move the tap and bucket forward."""

import hashlib
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / 'scripts/promote-release'
VERSION = '0.2.0-beta.10'
TAG = f'v{VERSION}'

FAKE_GH = r'''import json, os, shutil, subprocess, sys
args = sys.argv[1:]
fixtures = os.environ['FAKE_FIXTURES']
artifacts = {'railoxide-cask': 'railoxide.rb', 'railoxide-scoop-manifest': 'railoxide.json'}
if args == ['api', 'repos/triamazikamno/railoxide/releases?per_page=100']:
    print(open(os.path.join(fixtures, 'releases.json')).read())
elif args[:2] == ['run', 'list'] and args[args.index('--branch') + 1] == os.environ['FAKE_RUN_TAG']:
    print(json.dumps([{'databaseId': 42}]))
elif args[:2] == ['run', 'list']:
    print('[]')
elif args[:3] == ['run', 'download', '42'] and args[args.index('--name') + 1] in artifacts:
    name = artifacts[args[args.index('--name') + 1]]
    directory = args[args.index('--dir') + 1]
    os.makedirs(directory, exist_ok=True)
    shutil.copyfile(os.path.join(fixtures, name), os.path.join(directory, name))
elif args[:3] == ['repo', 'clone', 'triamazikamno/scoop-bucket'] and args[4:] == ['--', '-q']:
    sys.exit(subprocess.run(['git', 'clone', '-q', os.environ['FAKE_BUCKET_REMOTE'], args[3]]).returncode)
else:
    sys.exit(2)
'''

FAKE_CURL = r'''import os, shutil, sys
args = sys.argv[1:]
source = os.path.join(os.environ['FAKE_FIXTURES'], args[-1].rsplit('/', 1)[1])
if args[:2] != ['-fsSL', '-o'] or not os.path.exists(source):
    sys.exit(22)
shutil.copyfile(source, args[2])
'''

FAKE_BREW = r'''import os, subprocess, sys
args = sys.argv[1:]
tap = os.environ['FAKE_TAP']
if args == ['--repository', 'triamazikamno/tap']:
    print(tap)
elif args == ['tap', 'triamazikamno/tap']:
    sys.exit(subprocess.run(['git', 'clone', '-q', os.environ['FAKE_TAP_REMOTE'], tap]).returncode)
elif args[:2] in (['style', '--cask'], ['audit', '--cask']) and args[2:] == ['triamazikamno/tap/railoxide']:
    sys.exit(0 if os.path.exists(os.path.join(tap, 'Casks/railoxide.rb')) else 1)
else:
    sys.exit(2)
'''


def run(*args, cwd=None, env=None, check=True, stdin=None):
    result = subprocess.run(args, cwd=cwd, env=env, input=stdin, capture_output=True, text=True)
    if check and result.returncode != 0:
        raise AssertionError(f'{args} failed: {result.stderr}')
    return result


def load_script():
    loader = importlib.machinery.SourceFileLoader('promote_release', str(SCRIPT))
    module = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
    loader.exec_module(module)
    return module


class PromoteReleaseTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        temporary = tempfile.TemporaryDirectory()
        cls.addClassCleanup(temporary.cleanup)
        cls.gnupg = Path(temporary.name) / 'gnupg'
        cls.gnupg.mkdir(mode=0o700)
        run('gpg', '--batch', '--homedir', cls.gnupg, '--passphrase', '', '--quick-gen-key',
            'release <release@example.invalid>', 'ed25519', 'sign', 'never')
        cls.addClassCleanup(subprocess.run, ['gpgconf', '--homedir', cls.gnupg, '--kill', 'all'],
                            capture_output=True)

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.fixtures = self.root / 'fixtures'
        self.fixtures.mkdir()
        bin_dir = self.root / 'bin'
        bin_dir.mkdir()
        for name, source in [('gh', FAKE_GH), ('curl', FAKE_CURL), ('brew', FAKE_BREW)]:
            (bin_dir / name).write_text(f'#!{sys.executable}\n' + source)
            (bin_dir / name).chmod(0o755)
        self.tap = self.root / 'tap'
        self.remote = self.root / 'homebrew-tap.git'
        self.bucket_remote = self.root / 'scoop-bucket.git'
        self.env = {
            **os.environ,
            'PATH': f'{bin_dir}{os.pathsep}{os.environ["PATH"]}',
            'FAKE_FIXTURES': str(self.fixtures),
            'FAKE_RUN_TAG': TAG,
            'FAKE_TAP': str(self.tap),
            'FAKE_TAP_REMOTE': str(self.remote),
            'FAKE_BUCKET_REMOTE': str(self.bucket_remote),
            'GIT_CONFIG_GLOBAL': os.devnull,
            'GIT_CONFIG_NOSYSTEM': '1',
            'GIT_AUTHOR_NAME': 'Test', 'GIT_AUTHOR_EMAIL': 'test@example.invalid',
            'GIT_COMMITTER_NAME': 'Test', 'GIT_COMMITTER_EMAIL': 'test@example.invalid',
        }
        self.write_releases([('v0.2.0-beta.9', '2026-10-01T00:00:00Z', 1),
                             (TAG, '2026-10-20T00:00:00Z', 2),
                             ('v0.3.0', None, 3)])
        run('gpg', '--batch', '--homedir', self.gnupg, '--export', '-o',
            self.fixtures / 'flatpak-release-public.gpg')
        sums = self.publish(f'RailOxide-{TAG}-macos-arm64.dmg', f'RailOxide-{TAG}-macos-SHA256SUMS')
        self.cask = self.render(sums, self.fixtures / 'railoxide.rb')
        self.windows_sums = self.publish(f'RailOxide-{TAG}-windows-x64.zip', f'RailOxide-{TAG}-windows-SHA256SUMS')
        self.manifest = self.render_manifest(self.windows_sums, self.fixtures / 'railoxide.json')

        self.seed = self.root / 'seed'
        self.bucket_seed = self.root / 'bucket-seed'
        for seed, remote in [(self.seed, self.remote), (self.bucket_seed, self.bucket_remote)]:
            run('git', 'init', '-q', '--bare', '-b', 'main', remote, env=self.env)
            run('git', 'init', '-q', '-b', 'main', seed, env=self.env)
            (seed / 'README.md').write_text('repo\n')
            self.commit_seed(seed, remote)

    def write_releases(self, releases):
        (self.fixtures / 'releases.json').write_text(json.dumps([
            {'tag_name': tag, 'published_at': published, 'id': id, 'draft': published is None}
            for tag, published, id in releases
        ]))

    def publish(self, asset, sums_name):
        """Write a release asset with its signed checksum file into the fixtures."""
        (self.fixtures / asset).write_bytes(f'{asset} bytes'.encode())
        sums = self.fixtures / sums_name
        sums.write_text(f'{hashlib.sha256(f"{asset} bytes".encode()).hexdigest()}  {asset}\n')
        run('gpg', '--batch', '--homedir', self.gnupg, '--pinentry-mode', 'loopback', '--passphrase', '',
            '--armor', '--detach-sign', '-o', f'{sums}.asc', sums)
        return sums

    def render(self, sums, output, version=VERSION):
        run(sys.executable, ROOT / 'scripts/macos-packaging.py', 'render-cask', '--version', version,
            '--checksums', sums, '--output', output)
        return output.read_text()

    def render_manifest(self, sums, output, version=VERSION):
        run(sys.executable, ROOT / 'scripts/windows-packaging.py', 'render-manifest', '--version', version,
            '--checksums', sums, '--output', output)
        return output.read_bytes()

    def commit_seed(self, seed, remote):
        run('git', 'add', '-A', cwd=seed, env=self.env)
        run('git', 'commit', '-q', '-m', 'seed', cwd=seed, env=self.env)
        run('git', 'push', '-q', remote, 'HEAD:main', cwd=seed, env=self.env)

    def seed_bucket(self, manifest):
        (self.bucket_seed / 'bucket').mkdir(exist_ok=True)
        (self.bucket_seed / 'bucket/railoxide.json').write_bytes(manifest)
        self.commit_seed(self.bucket_seed, self.bucket_remote)

    def promote(self, *args, answer='y\n'):
        return run(SCRIPT, *args, env=self.env, check=False, stdin=answer)

    def git(self, *args, cwd=None):
        return run('git', *args, cwd=cwd or self.tap, env=self.env).stdout

    def test_promotes_verified_cask_of_latest_release(self):
        result = self.promote('homebrew')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.git('show', 'main:Casks/railoxide.rb', cwd=self.remote), self.cask)
        self.assertEqual(self.git('log', '-1', '--format=%s', 'main', cwd=self.remote), f'railoxide {VERSION}\n')
        self.assertEqual(self.git('status', '--porcelain'), '')

    def test_refused_promotions_leave_tap_unchanged(self):
        wrong_sums = self.root / 'wrong-SHA256SUMS'
        wrong_sums.write_text(f'{"0" * 64}  RailOxide-{TAG}-macos-arm64.dmg\n')
        newer_sums = self.root / 'newer-SHA256SUMS'
        newer_sums.write_text(f'{"1" * 64}  RailOxide-v0.2.0-macos-arm64.dmg\n')
        (self.seed / 'Casks').mkdir()
        self.render(newer_sums, self.seed / 'Casks/railoxide.rb', '0.2.0')
        self.commit_seed(self.seed, self.remote)
        run('git', 'clone', '-q', self.remote, self.tap, env=self.env)
        head = self.git('rev-parse', 'HEAD')
        tapped = (self.tap / 'Casks/railoxide.rb').read_text()

        mismatched = self.render(wrong_sums, self.root / 'mismatched.rb')
        hooked = self.cask.replace('\nend\n', '\n  postflight do\n    system "/usr/bin/true"\n  end\nend\n')
        self.assertNotEqual(hooked, self.cask)
        for label, cask, reason in [
            ('cask digest differs from signed checksum', mismatched, 'differs from the railoxide.rb'),
            ('cask adds code beyond the template', hooked, 'differs from the railoxide.rb'),
            ('tap already has a newer version', self.cask, 'not older than'),
        ]:
            with self.subTest(label):
                (self.fixtures / 'railoxide.rb').write_text(cask)
                result = self.promote('homebrew')
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn(reason, result.stderr)
                self.assertEqual(self.git('rev-parse', 'HEAD'), head)
                self.assertEqual(self.git('rev-parse', 'main', cwd=self.remote), head)
                self.assertEqual(self.git('status', '--porcelain'), '')
                self.assertEqual((self.tap / 'Casks/railoxide.rb').read_text(), tapped)

    def test_tag_other_than_latest_is_refused(self):
        result = self.promote('homebrew', 'v0.2.0-beta.9')
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn('not the latest release', result.stderr)
        self.assertFalse(self.tap.exists())

    def test_promotes_verified_manifest_of_latest_release(self):
        result = self.promote('scoop')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        promoted = subprocess.run(['git', 'show', 'main:bucket/railoxide.json'], cwd=self.bucket_remote,
                                  env=self.env, capture_output=True, check=True).stdout
        self.assertEqual(promoted, self.manifest)
        self.assertEqual(self.git('log', '-1', '--format=%s', 'main', cwd=self.bucket_remote),
                         f'railoxide {VERSION}\n')

    def test_unsafe_manifests_are_not_promoted(self):
        head = self.git('rev-parse', 'main', cwd=self.bucket_remote)
        wrong_sums = self.root / 'wrong-SHA256SUMS'
        wrong_sums.write_text(f'{"0" * 64}  RailOxide-{TAG}-windows-x64.zip\n')
        expanded = json.loads(self.manifest)
        # Scoop expands env_set values as PowerShell strings during installation.
        expanded['env_set'] = {'RAILOXIDE_REVIEW': '$(Write-Output proof)'}
        for label, manifest in [
            ('manifest hash differs from signed checksum', self.render_manifest(wrong_sums, self.root / 'wrong.json')),
            ('manifest adds a field beyond the template', json.dumps(expanded, indent=2).encode() + b'\n'),
        ]:
            with self.subTest(label):
                (self.fixtures / 'railoxide.json').write_bytes(manifest)
                result = self.promote('scoop')
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn('differs from the railoxide.json', result.stderr)
                self.assertEqual(self.git('rev-parse', 'main', cwd=self.bucket_remote), head)

    def test_bucket_with_same_or_newer_version_is_refused(self):
        wrong_sums = self.root / 'wrong-SHA256SUMS'
        wrong_sums.write_text(f'{"0" * 64}  RailOxide-{TAG}-windows-x64.zip\n')
        newer_sums = self.root / 'newer-SHA256SUMS'
        newer_sums.write_text(f'{"1" * 64}  RailOxide-v0.2.0-windows-x64.zip\n')
        for label, bucketed in [
            ('bucket has a different manifest of the same version',
             self.render_manifest(wrong_sums, self.root / 'same.json')),
            ('bucket has a newer version', self.render_manifest(newer_sums, self.root / 'newer.json', '0.2.0')),
        ]:
            with self.subTest(label):
                self.seed_bucket(bucketed)
                head = self.git('rev-parse', 'main', cwd=self.bucket_remote)
                result = self.promote('scoop')
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn('not older than', result.stderr)
                self.assertEqual(self.git('rev-parse', 'main', cwd=self.bucket_remote), head)

    def test_no_target_promotes_cask_then_manifest(self):
        result = self.promote(answer='y\ny\n')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for remote in (self.remote, self.bucket_remote):
            self.assertEqual(self.git('log', '-1', '--format=%s', 'main', cwd=remote), f'railoxide {VERSION}\n')

    def test_version_ordering(self):
        key = load_script().version_key
        for older, newer in [('0.2.0-beta.9', '0.2.0-beta.10'), ('0.2.0-beta.10', '0.2.0-beta.10.1'),
                             ('0.2.0-beta.12', '0.2.0'), ('0.2.0-1', '0.2.0-beta'),
                             ('0.9.9', '0.10.0'), ('1.0.0+zzz', '1.0.1+aaa')]:
            with self.subTest(older=older, newer=newer):
                self.assertLess(key(older), key(newer))
        self.assertEqual(key('1.0.0+a'), key('1.0.0+b'))


if __name__ == '__main__':
    unittest.main()
