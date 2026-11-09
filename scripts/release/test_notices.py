"""Release acceptance/rejection tests; no network or Rust required."""
import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import notices
from license_policy import canonical, sha256, license_policy, check_crate, satisfies
from native_sources import check_native, native_inventory, package_hash


def fixture_bundle(target):
    """A synthetic permitted crate for archive tests, not dependency evidence."""
    bundle = {
        'schema': 'aelix-studio-host.third-party-notices/1', 'rustTarget': target,
        'denySha256': sha256((HERE.parents[1] / 'deny.toml').read_bytes()),
        'policySha256': {p: sha256((HERE / p).read_bytes()) for p in notices.POLICIES},
        'crates': [{'name': 'release-test-fixture', 'version': '1.0.0', 'source': 'synthetic fixture',
                    'authors': ['Fixture author'], 'license': 'MIT', 'selectedLicenses': ['MIT'],
                    'licenseTexts': {'MIT': ['Fixture license text']},
                    'originals': [notices.original('LICENSE', b'Copyright fixture\nFixture license text\n')]}],
        'nativeSources': [], 'apacheNoticeFiles': [], 'missingNotices': [],
    }
    text, raw = notices.render(bundle), canonical(bundle)
    version, spec = notices.tool_spec(target)
    result = {'status': 'passed', 'rustTarget': target, 'exitCode': 0, 'errorDiagnostics': 0,
              'tool': {'name': 'cargo-about', 'version': version, 'archiveSha256': spec['sha256']},
              'generatorSha256': sha256((HERE / 'notices.py').read_bytes()),
              'helperSha256': {p: sha256((HERE / p).read_bytes()) for p in ('license_policy.py', 'native_sources.py')},
              'policySha256': bundle['policySha256'], 'denySha256': bundle['denySha256'],
              'crateCount': 1, 'nativePackageCount': 0, 'missingNotices': [],
              'files': {name: sha256(data) for name, data in zip(notices.FILES, (text, raw))}}
    return text, raw, result


class LicensePolicyTests(unittest.TestCase):
    def test_nested_alternatives_and_required_conditions(self):
        self.assertTrue(satisfies('ISC AND (Apache-2.0 OR MIT)', {'ISC', 'MIT'}))
        self.assertFalse(satisfies('ISC AND (Apache-2.0 OR MIT)', {'MIT'}))
        self.assertTrue(satisfies('Apache-2.0 WITH LLVM-exception OR MIT', {'Apache-2.0 WITH LLVM-exception'}))
        self.assertFalse(satisfies('Apache-2.0 WITH LLVM-exception', {'Apache-2.0'}))
        for expression in ('MIT XOR BSD', '(MIT', 'MIT OR', 'MIT & GPL', ''):
            with self.subTest(expression), self.assertRaises(ValueError):
                satisfies(expression, {'MIT'})

    def test_deny_policy_rejects_disallowed_and_incomplete_choices(self):
        package = {'name': 'fixture', 'version': '1.0.0', 'license': 'MIT AND BSD-3-Clause'}
        check_crate(package, {'MIT', 'BSD-3-Clause'}, license_policy())
        with self.assertRaisesRegex(ValueError, 'do not satisfy metadata'):
            check_crate(package, {'MIT'}, license_policy())
        with self.assertRaisesRegex(ValueError, 'outside deny.toml'):
            check_crate(dict(package, license='GPL-3.0-only'), {'GPL-3.0-only'}, license_policy())

    def test_bzip2_exception_is_exact_version_and_name(self):
        package = {'name': 'libbz2-rs-sys', 'version': '0.2.6', 'license': 'bzip2-1.0.6'}
        check_crate(package, {'bzip2-1.0.6'}, license_policy())
        for field, value in (('name', 'another-crate'), ('version', '0.2.7')):
            with self.assertRaisesRegex(ValueError, 'outside deny.toml'):
                check_crate(dict(package, **{field: value}), {'bzip2-1.0.6'}, license_policy())

    def test_unsupported_version_exception_is_not_widened(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'deny.toml'
            path.write_text('[licenses]\nprivate = { ignore = true }\nallow = ["MIT"]\n[[licenses.exceptions]]\nname="x"\nversion="^1"\nallow=["ISC"]\n')
            with self.assertRaisesRegex(ValueError, 'exact name and =version'):
                license_policy(path)

    def test_zero_exit_with_parse_error_is_failure(self):
        for error in ('[ERROR] failed to parse license GPL-2.0', '\x1b[31mERROR\x1b[0m invalid expression', 'error: malformed config'):
            process = subprocess.CompletedProcess([], 0, '{}', error)
            with mock.patch.object(notices.subprocess, 'run', return_value=process), self.assertRaisesRegex(ValueError, 'License tool failed'):
                notices.checked_tool_run(['tool'])
        process = subprocess.CompletedProcess([], 1, '{}', 'failure')
        with mock.patch.object(notices.subprocess, 'run', return_value=process), self.assertRaisesRegex(ValueError, 'exit 1'):
            notices.checked_tool_run(['tool'])

    def test_bad_tool_checksum_is_rejected_before_execution(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'tool.tar.gz').write_bytes(b'untrusted archive')
            with self.assertRaisesRegex(ValueError, 'archive checksum mismatch'):
                notices.extract_tool(root / 'tool.tar.gz', 'aarch64-apple-darwin', root)


class NativeSourceTests(unittest.TestCase):
    def fixture(self, temporary):
        root = Path(temporary) / 'registry/src/index/native-fixture-1.0.0'
        root.mkdir(parents=True)
        (root / 'library.c').write_bytes(b'/* Copyright fixture */\nint f(void) { return 1; }\n')
        (root / 'COPYING').write_bytes(b'Original native terms\n')
        archive = Path(temporary) / 'registry/cache/index/native-fixture-1.0.0.crate'
        archive.parent.mkdir(parents=True)
        archive.write_bytes(b'fixture source archive')
        package = {'name': 'native-fixture', 'version': '1.0.0', 'links': 'fixture'}
        files = native_inventory(root)
        policy = {'packages': [{'crate': 'native-fixture', 'version': '1.0.0', 'links': 'fixture',
                               'sourceArchiveSha256': sha256(archive.read_bytes()),
                               'packagedFilesSha256': package_hash(root),
                               'nativeFilesSha256': sha256(canonical(files)),
                               'buildScriptsSha256': sha256(canonical([])),
                               'components': [{'component': 'fixture', 'version': '1.0.0',
                                               'licenseConditions': 'Original native terms',
                                               'sourcePrefixes': [''], 'sourceSha256': sha256(canonical(files)),
                                               'notices': [{'path': 'COPYING', 'sha256': sha256((root / 'COPYING').read_bytes())}]}]}]}
        return root, package, policy

    def test_originals_and_actual_system_or_static_linkage(self):
        with tempfile.TemporaryDirectory() as temporary:
            root, package, policy = self.fixture(temporary)
            result = check_native(package, root, policy, [{'linked_libs': ['static=fixture']}])
            self.assertEqual(result['linkage'], 'static')
            self.assertEqual(result['notices'][0]['text'], 'Original native terms\n')
            result = check_native(package, root, policy, [{'linked_libs': ['fixture']}])
            self.assertEqual(result['linkage'], 'system-or-dynamic')
            self.assertEqual(len(result['notices']), 1)

    def test_unknown_package_version_and_added_or_changed_c_source_fail(self):
        for case in ('package', 'version', 'add', 'modify', 'remove-notice', 'empty-notice-policy'):
            with self.subTest(case), tempfile.TemporaryDirectory() as temporary:
                root, package, policy = self.fixture(temporary)
                if case == 'package':
                    package['name'] = 'unlisted'
                    expected = 'Unlisted native source'
                elif case == 'version':
                    package['version'] = '1.0.1'
                    expected = 'version mismatch'
                elif case == 'add':
                    (root / 'new.c').write_text('int new;')
                    expected = 'source hash mismatch'
                elif case == 'modify':
                    (root / 'library.c').write_text('int changed;')
                    expected = 'source hash mismatch'
                elif case == 'remove-notice':
                    (root / 'COPYING').unlink()
                    expected = 'source hash mismatch'
                else:
                    policy['packages'][0]['components'][0]['notices'] = []
                    expected = 'no original license notice'
                with self.assertRaisesRegex(ValueError, expected):
                    check_native(package, root, policy)

    def test_git_root_copyright_and_nested_apache_notice_are_retained(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / '.git').mkdir()
            (root / 'LICENSE').write_text('Copyright upstream owner\nApache original\n')
            package = root / 'crates/pkg'
            package.mkdir(parents=True)
            (package / 'NOTICE.txt').write_text('Apache attribution\n')
            (package / 'source.rs').write_text('// Copyright component owner\nfn f() {}\n')
            items = notices.package_originals({'name': 'fixture', 'version': '1.0.0', 'source': 'git+fixture',
                                               'manifest_path': str(package / 'Cargo.toml')}, {'files': []})
            paths = {item['path'] for item in items}
            self.assertIn('repository-root/LICENSE', paths)
            self.assertIn('NOTICE.txt', paths)
            self.assertTrue(any('copyright-' in path for path in paths))
            self.assertIn('Copyright upstream owner', '\n'.join(notices.decode_original(x) for x in items))


class BundleValidationTests(unittest.TestCase):
    def test_fixture_validates_for_every_target(self):
        for target in json.loads((HERE / 'policy.json').read_text())['targets'].values():
            text, raw, result = fixture_bundle(target['rustTarget'])
            self.assertEqual(notices.validate_bundle(text, raw, result, target['rustTarget'])['missingNotices'], [])

    def test_tamper_and_failed_or_incomplete_generation_are_rejected(self):
        target = 'x86_64-unknown-linux-gnu'
        for case in ('text', 'json', 'tool', 'policy', 'target', 'source-audit-only', 'error-diagnostic', 'missing-notice'):
            text, raw, result = fixture_bundle(target)
            if case == 'text': text += b'changed'
            elif case == 'json': raw += b' '
            elif case == 'tool': result['tool']['archiveSha256'] = '0' * 64
            elif case == 'policy': result['policySha256'] = {}
            elif case == 'target': result['rustTarget'] = 'wrong'
            elif case == 'source-audit-only': result['status'] = 'source-audit-only'
            elif case == 'error-diagnostic': result['errorDiagnostics'] = 1
            elif case == 'missing-notice': result['missingNotices'] = ['COPYING']
            with self.subTest(case), self.assertRaises(ValueError):
                notices.validate_bundle(text, raw, result, target)

    def test_hash_rewrite_does_not_allow_disallowed_license_or_missing_original(self):
        target = 'x86_64-unknown-linux-gnu'
        for case in ('license', 'original', 'license-text'):
            text, raw, result = fixture_bundle(target)
            bundle = json.loads(raw)
            package = bundle['crates'][0]
            if case == 'license':
                package['license'] = 'GPL-3.0-only'
                package['selectedLicenses'] = ['GPL-3.0-only']
            elif case == 'original': package['originals'] = []
            else: package['licenseTexts'] = {}
            if case == 'license': package['licenseTexts'] = {'GPL-3.0-only': ['GPL text']}
            with self.subTest(case), self.assertRaises(ValueError):
                text, raw = notices.render(bundle), canonical(bundle)
                result['files'] = {p: sha256(data) for p, data in zip(notices.FILES, (text, raw))}
                notices.validate_bundle(text, raw, result, target)


if __name__ == '__main__':
    unittest.main()
