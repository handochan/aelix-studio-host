"""Unit tests for the release scripts (stdlib unittest; no Rust toolchain needed)."""
import hashlib
import json
from pathlib import Path
import re
import sys
import tarfile
import tempfile
import unittest
from unittest import mock
import zipfile

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import build  # noqa: E402
import identity  # noqa: E402
import provenance  # noqa: E402
import smoke  # noqa: E402

ROOT = HERE.parents[1]
POLICY = json.loads((HERE / 'policy.json').read_text())
COMMIT = 'a' * 40


class ArchiveTests(unittest.TestCase):
    def test_archive_names_follow_the_studio_resolver_pattern(self):
        self.assertEqual(build.archive_name('host-v0.2.2-aelix.1', 'aarch64-apple-darwin'),
                         'aioncore-host-v0.2.2-aelix.1-aarch64-apple-darwin.tar.gz')
        self.assertEqual(build.archive_name('dryrun-0123456789ab', 'x86_64-pc-windows-msvc'),
                         'aioncore-dryrun-0123456789ab-x86_64-pc-windows-msvc.zip')
        for label in ('', '../x', 'a b', 'v1/2', '-leading'):
            with self.assertRaises(ValueError):
                build.archive_name(label, 'x86_64-unknown-linux-gnu')

    def test_archives_are_deterministic_and_ordered(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'aioncore').write_bytes(b'binary')
            (root / 'NOTICE').write_bytes(b'notice')
            members = [('aioncore', root / 'aioncore', 0o755), ('NOTICE', root / 'NOTICE', 0o644)]
            for extension in ('.tar.gz', '.zip'):
                first, second = root / ('a' + extension), root / ('b' + extension)
                build.write_archive(first, members, 1_700_000_000)
                build.write_archive(second, members, 1_700_000_000)
                self.assertEqual(first.read_bytes(), second.read_bytes())
                self.assertEqual(provenance.member_names(first), ['aioncore', 'NOTICE'])
                self.assertEqual(provenance.read_member(first, 'aioncore'), b'binary')
            with tarfile.open(root / 'a.tar.gz') as archive:
                info = archive.getmember('aioncore')
                self.assertEqual((info.mode, info.uid, info.gid, info.mtime), (0o755, 0, 0, 1_700_000_000))
            with zipfile.ZipFile(root / 'a.zip') as archive:
                self.assertEqual(archive.getinfo('aioncore').external_attr >> 16, 0o100755)

    def test_zip_timestamps_are_clamped_to_1980(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'f').write_bytes(b'x')
            build.write_archive(root / 'a.zip', [('f', root / 'f', 0o644)], 0)
            with zipfile.ZipFile(root / 'a.zip') as archive:
                self.assertEqual(archive.getinfo('f').date_time, (1980, 1, 1, 0, 0, 0))


class CheckoutBytesTests(unittest.TestCase):
    def test_line_ending_rewrites_are_refused(self):
        import subprocess
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            subprocess.run(['git', 'init', '-q'], cwd=root, check=True)
            (root / 'migration.sql').write_bytes(b'CREATE TABLE t (id INTEGER);\n')
            subprocess.run(['git', 'add', 'migration.sql'], cwd=root, check=True)
            with mock.patch.object(build, 'ROOT', root):
                self.assertEqual(build.verify_checkout_bytes(), 1)
                (root / 'migration.sql').write_bytes(b'CREATE TABLE t (id INTEGER);\r\n')
                with self.assertRaises(ValueError):
                    build.verify_checkout_bytes()


class LabelTests(unittest.TestCase):
    def label(self, event, ref, on_main=True, version='0.2.2'):
        with mock.patch.object(provenance, 'is_ancestor', return_value=on_main), \
                mock.patch.object(provenance, 'workspace_version', return_value=version):
            return provenance.release_label(event, ref, COMMIT, 'origin/main')

    def test_only_aelix_tags_on_main_publish(self):
        self.assertEqual(self.label('push', 'refs/tags/host-v0.2.2-aelix.1'), ('host-v0.2.2-aelix.1', True))
        self.assertEqual(self.label('pull_request', 'refs/pull/7/merge'), ('dryrun-aaaaaaaaaaaa', False))
        self.assertEqual(self.label('workflow_dispatch', 'refs/heads/feature'), ('dryrun-aaaaaaaaaaaa', False))

    def test_upstream_and_malformed_tags_are_refused(self):
        for tag in ('v0.2.2', 'v0.2.3', 'host-v0.2.2', 'host-v0.2.2-aelix.0', 'host-v0.2.2-aelix.01',
                    'host-v0.2.2-aelix.1-rc', 'host-v0.2.2-beta.1'):
            with self.assertRaises(ValueError, msg=tag):
                self.label('push', 'refs/tags/' + tag)

    def test_tag_version_must_match_the_workspace_and_be_on_main(self):
        with self.assertRaises(ValueError):
            self.label('push', 'refs/tags/host-v0.2.3-aelix.1')
        with self.assertRaises(ValueError):
            self.label('push', 'refs/tags/host-v0.2.2-aelix.1', on_main=False)
        with self.assertRaises(ValueError):
            self.label('workflow_dispatch', 'refs/tags/host-v0.2.2-aelix.1')

    def test_workspace_version_is_read_from_the_workspace_table(self):
        self.assertRegex(provenance.workspace_version(), r'^\d+\.\d+\.\d+$')


class AssembleTests(unittest.TestCase):
    def make_target(self, source, name, spec, label, binary_bytes=b'host', smoke_status='passed'):
        directory = source / ('release-' + name)
        directory.mkdir(parents=True)
        binary = 'aioncore.exe' if name.startswith('win32-') else 'aioncore'
        (directory / binary).write_bytes(binary_bytes)
        receipt = {
            'status': 'built', 'commit': COMMIT, 'tree': 'b' * 40, 'dirty': False, 'label': label, 'target': name,
            'rustTarget': spec['rustTarget'], 'upstream': POLICY['upstream'], 'binary': binary,
            'binarySha256': hashlib.sha256(binary_bytes).hexdigest(),
            'binaryCodeSha256': hashlib.sha256(binary_bytes).hexdigest(), 'rustFlags': spec.get('rustFlags', ''),
            'cargoVersion': 'cargo 1.95.0', 'rustcVersionVerbose': 'rustc 1.95.0', 'rustToolchain': '1.95.0',
            'cargoLockSha256': 'c' * 64, 'licenseSha256': 'd' * 64, 'noticeSha256': 'e' * 64,
            'policySha256': 'f' * 64, 'buildRecipeSha256': '0' * 64, 'checkoutFilesVerified': 1440,
            'archive': build.archive_name(label, spec['rustTarget']),
        }
        (directory / 'build-receipt.json').write_text(json.dumps(receipt))
        members = [(binary, directory / binary, 0o755), ('LICENSE', ROOT / 'LICENSE', 0o644),
                   ('NOTICE', ROOT / 'NOTICE', 0o644), ('build-receipt.json', directory / 'build-receipt.json', 0o644)]
        build.write_archive(directory / receipt['archive'], members, 1_700_000_000)
        (directory / 'smoke.json').write_text(json.dumps({
            'status': smoke_status, 'binarySha256': receipt['binarySha256'], 'checks': ['version'],
            'version': 'aioncore 0.2.2'}))
        return directory

    def assemble(self, source, output, label='host-v0.2.2-aelix.1'):
        with mock.patch.object(provenance, 'check_upstream', return_value=3), \
                mock.patch.object(provenance, 'workspace_version', return_value='0.2.2'):
            return provenance.assemble(label, source, output, COMMIT, {'GITHUB_RUN_ID': '42'})

    def test_assembles_checksums_and_provenance_for_all_targets(self):
        with tempfile.TemporaryDirectory() as temporary:
            source, output = Path(temporary) / 'in', Path(temporary) / 'out'
            for name, spec in POLICY['targets'].items():
                self.make_target(source, name, spec, 'host-v0.2.2-aelix.1')
            result = self.assemble(source, output)
            self.assertEqual(result['kind'], 'release')
            self.assertEqual(sorted(result['targets']), sorted(POLICY['targets']))
            self.assertEqual(result['upstream']['commitsSinceUpstream'], 3)
            self.assertTrue(result['build']['runUrl'].endswith('/actions/runs/42'))
            lines = (output / provenance.CHECKSUMS).read_text().splitlines()
            self.assertEqual(len(lines), len(POLICY['targets']) + 1)
            for line in lines:
                expected, name = line.split('  ')
                self.assertEqual(hashlib.sha256((output / name).read_bytes()).hexdigest(), expected)
            self.assertIn('host-v0.2.2-aelix.1', provenance.notes(result))

    def test_refuses_a_failed_smoke_a_missing_target_or_a_tampered_archive(self):
        cases = {
            'smoke': lambda source: self.make_target(source, 'linux-x64', POLICY['targets']['linux-x64'],
                                                     'host-v0.2.2-aelix.1', smoke_status='failed'),
            'missing': lambda source: None,
        }
        for case, last in cases.items():
            with self.subTest(case), tempfile.TemporaryDirectory() as temporary:
                source = Path(temporary) / 'in'
                for name, spec in POLICY['targets'].items():
                    if name != 'linux-x64':
                        self.make_target(source, name, spec, 'host-v0.2.2-aelix.1')
                last(source)
                with self.assertRaises((ValueError, FileNotFoundError)):
                    self.assemble(source, Path(temporary) / 'out')
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / 'in'
            for name, spec in POLICY['targets'].items():
                directory = self.make_target(source, name, spec, 'host-v0.2.2-aelix.1')
            receipt = json.loads((directory / 'build-receipt.json').read_text())
            receipt['binarySha256'] = '1' * 64
            (directory / 'build-receipt.json').write_text(json.dumps(receipt))
            with self.assertRaises(ValueError):
                self.assemble(source, Path(temporary) / 'out')


class ConsistencyTests(unittest.TestCase):
    def test_workflow_matrix_deny_targets_and_policy_agree(self):
        workflow = (ROOT / '.github/workflows/release.yml').read_text()
        self.assertEqual(sorted(re.findall(r'\{ target: ([a-z0-9-]+),', workflow)), sorted(POLICY['targets']))
        deny = (ROOT / 'deny.toml').read_text().split('targets = [', 1)[1].split(']', 1)[0]
        self.assertEqual(sorted(re.findall(r'"([a-z0-9_]+-[a-z0-9_-]+)"', deny)),
                         sorted(spec['rustTarget'] for spec in POLICY['targets'].values()))

    def test_release_trigger_cannot_match_upstream_tags(self):
        workflow = (ROOT / '.github/workflows/release.yml').read_text()
        trigger = workflow.split('\non:', 1)[1].split('\npermissions:', 1)[0]
        self.assertIn('tags: ["host-v*"]', trigger)
        self.assertEqual(len(re.findall(r'tags:', trigger)), 1)

    def test_policy_upstream_base_matches_the_toolchain_and_license_files(self):
        self.assertEqual(len(POLICY['upstream']['commit']), 40)
        self.assertTrue((ROOT / 'LICENSE').read_text().startswith(' ' * 33 + 'Apache License'))
        self.assertTrue(build.toolchain_channel())


class SmokeAndIdentityTests(unittest.TestCase):
    def test_listening_event_requires_a_complete_loopback_line(self):
        self.assertIsNone(smoke.listening_port(b'AIONCORE_LISTENING {"host":"127.0.0.1","port":5}'))
        self.assertEqual(smoke.listening_port(b'log\nAIONCORE_LISTENING {"host":"127.0.0.1","port":5}\n'), 5)
        with self.assertRaises(RuntimeError):
            smoke.listening_port(b'AIONCORE_LISTENING {"host":"0.0.0.0","port":5}\n')

    def test_macho_identity_rejects_other_formats(self):
        with self.assertRaises(ValueError):
            identity.macho_code_identity(b'\x7fELF' + b'\0' * 64)


if __name__ == '__main__':
    unittest.main()
