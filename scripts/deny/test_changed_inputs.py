"""Advisory trigger decisions against real git repositories."""
import contextlib
import io
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import changed_inputs  # noqa: E402

ZERO = '0' * 40
GIT_ENVIRONMENT = {
    'GIT_AUTHOR_NAME': 'Test', 'GIT_AUTHOR_EMAIL': 'test@example.com',
    'GIT_COMMITTER_NAME': 'Test', 'GIT_COMMITTER_EMAIL': 'test@example.com',
    'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_CONFIG_NOSYSTEM': '1',
}


class Repository:
    """A repository whose `main` holds every dependency input, checked out on `feature`."""

    def __init__(self, root):
        self.root = Path(root)
        self.git('init', '-q', '-b', 'main')
        for name in ('Cargo.lock', 'deny.toml', 'Cargo.toml', 'crates/a/Cargo.toml', 'crates/a/src/lib.rs',
                     '.github/actions/cargo-deny/action.yml', '.github/workflows/ci.yml', 'README.md'):
            self.write(name, 'seed\n')
        self.main = self.commit('seed')
        self.publish_main()
        self.git('checkout', '-q', '-b', 'feature')

    def git(self, *arguments):
        environment = {**os.environ, **GIT_ENVIRONMENT}
        return subprocess.run(['git', *arguments], cwd=self.root, check=True, capture_output=True, text=True,
                              env=environment).stdout.strip()

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self, message):
        self.git('add', '-A')
        self.git('commit', '-q', '-m', message)
        return self.git('rev-parse', 'HEAD')

    def change(self, name, text='changed\n'):
        self.write(name, text)
        return self.commit('change ' + name)

    def publish_main(self):
        self.git('update-ref', 'refs/remotes/origin/main', 'refs/heads/main')

    def decide(self, event='push', ref='refs/heads/feature', **arguments):
        return changed_inputs.decide(event, ref, cwd=self.root, **arguments)


class ChangedInputsTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        patch = mock.patch.dict(os.environ, GIT_ENVIRONMENT)
        patch.start()
        self.addCleanup(patch.stop)
        self.repository = Repository(temporary.name)

    def test_branch_without_dependency_input_changes_skips_advisories(self):
        self.repository.change('README.md')
        self.repository.change('crates/a/src/lib.rs')
        self.repository.change('.github/workflows/ci.yml')
        self.assertEqual(self.repository.decide(),
                         (False, 'no dependency input changed in commits not on origin/main'))

    def test_each_dependency_input_triggers_advisories(self):
        for name in ('Cargo.lock', 'deny.toml', 'Cargo.toml', 'crates/a/Cargo.toml',
                     '.github/actions/cargo-deny/action.yml'):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as root:
                repository = Repository(root)
                repository.change(name)
                self.assertEqual(repository.decide(), (True, name + ' changed in commits not on origin/main'))

    def test_same_names_outside_the_input_paths_do_not_trigger(self):
        self.repository.change('docs/deny.toml')
        self.repository.change('crates/a/Cargo.lock')
        self.repository.change('docs/Cargo.toml.md')
        self.assertFalse(self.repository.decide()[0])

    def test_added_and_deleted_inputs_trigger(self):
        self.repository.change('crates/b/Cargo.toml')
        self.assertEqual(self.repository.decide()[0], True)
        self.repository.git('rm', '-q', 'crates/b/Cargo.toml', 'deny.toml')
        self.repository.commit('remove')
        self.assertEqual(self.repository.decide(), (True, 'deny.toml changed in commits not on origin/main'))

    def test_branch_is_compared_as_a_whole_not_per_push(self):
        self.repository.change('Cargo.lock')
        before = self.repository.change('README.md')
        self.repository.change('docs/notes.md')
        # A later push that only touches documentation still tests a branch that changes the lockfile.
        self.assertEqual(self.repository.decide(before=before)[0], True)

    def test_new_branch_push_has_no_previous_commit_and_still_compares_with_main(self):
        self.repository.change('README.md')
        self.assertEqual(self.repository.decide(before=ZERO)[0], False)

    def test_main_changes_after_the_branch_point_are_not_attributed_to_the_branch(self):
        self.repository.change('README.md')
        self.repository.git('checkout', '-q', 'main')
        self.repository.change('Cargo.lock')
        self.repository.publish_main()
        self.repository.git('checkout', '-q', 'feature')
        self.assertEqual(self.repository.decide()[0], False)

    def test_pull_request_uses_its_own_commits_on_the_merge_checkout(self):
        head = self.repository.change('README.md')
        self.repository.git('checkout', '-q', 'main')
        base = self.repository.change('Cargo.lock')
        self.repository.publish_main()
        # actions/checkout builds this merge of the head into the base.
        self.repository.git('merge', '-q', '--no-ff', '-m', 'merge', 'feature')
        self.assertEqual(self.repository.decide('pull_request', 'refs/pull/1/merge', pr_base=base, pr_head=head),
                         (False, 'no dependency input changed in the pull request'))

        self.repository.git('checkout', '-q', 'feature')
        head = self.repository.change('deny.toml')
        self.assertEqual(self.repository.decide('pull_request', 'refs/pull/1/merge', pr_base=base, pr_head=head),
                         (True, 'deny.toml changed in the pull request'))

    def test_pull_request_without_usable_commits_checks_advisories(self):
        head = self.repository.change('README.md')
        for arguments in ({}, {'pr_base': self.repository.main}, {'pr_base': 'main', 'pr_head': head},
                          {'pr_base': self.repository.main, 'pr_head': 'f' * 40},
                          {'pr_base': '--output=/tmp/x', 'pr_head': head}):
            with self.subTest(arguments=arguments):
                changed, reason = self.repository.decide('pull_request', 'refs/pull/1/merge', **arguments)
                self.assertTrue(changed)
                self.assertIn('cannot determine the change under test', reason)

    def test_push_to_main_is_compared_with_the_replaced_commit(self):
        self.repository.git('checkout', '-q', 'main')
        before = self.repository.main
        self.repository.change('README.md')
        self.repository.publish_main()
        self.assertEqual(self.repository.decide(ref='refs/heads/main', before=before),
                         (False, 'no dependency input changed in this push to main'))
        self.repository.change('Cargo.lock')
        self.repository.change('docs/notes.md')
        self.repository.publish_main()
        self.assertEqual(self.repository.decide(ref='refs/heads/main', before=before),
                         (True, 'Cargo.lock changed in this push to main'))

    def test_push_to_main_without_a_known_previous_commit_checks_advisories(self):
        self.repository.git('checkout', '-q', 'main')
        self.repository.change('README.md')
        self.repository.publish_main()
        for before in ('', ZERO, 'f' * 40, 'HEAD~1'):
            with self.subTest(before=before):
                changed, reason = self.repository.decide(ref='refs/heads/main', before=before)
                self.assertTrue(changed)
                self.assertIn('cannot determine the change under test', reason)

    def test_manual_run_on_main_has_nothing_to_compare(self):
        self.repository.git('checkout', '-q', 'main')
        self.assertEqual(self.repository.decide('workflow_dispatch', 'refs/heads/main')[0], False)

    def test_missing_default_branch_checks_advisories(self):
        self.repository.change('README.md')
        self.repository.git('update-ref', '-d', 'refs/remotes/origin/main')
        self.assertEqual(self.repository.decide()[0], True)
        self.assertEqual(self.repository.decide(default_branch='--all')[0], True)

    def test_many_changed_inputs_are_summarised(self):
        for index in range(7):
            self.repository.write('crates/c%d/Cargo.toml' % index, 'new\n')
        self.repository.commit('add crates')
        self.assertEqual(self.repository.decide(), (
            True, 'crates/c0/Cargo.toml, crates/c1/Cargo.toml, crates/c2/Cargo.toml, crates/c3/Cargo.toml, '
                  'crates/c4/Cargo.toml and 2 more changed in commits not on origin/main'))

    def test_command_prints_github_output_lines(self):
        self.repository.change('Cargo.lock')
        output = io.StringIO()
        argv = ['changed_inputs.py', '--event', 'push', '--ref', 'refs/heads/feature', '--before', ZERO]
        self.addCleanup(os.chdir, os.getcwd())
        os.chdir(self.repository.root)
        with mock.patch.object(sys, 'argv', argv), contextlib.redirect_stdout(output):
            changed_inputs.main()
        self.assertEqual(output.getvalue(),
                         'changed=true\nreason=Cargo.lock changed in commits not on origin/main\n')


if __name__ == '__main__':
    unittest.main()
