"""Structure of the release workflow's advisory gate, and the reader the checks rely on.

GitHub decides whether the publish job runs from the whole parsed workflow, so
these tests read it the same way: a condition is checked as the complete
scalar, however it is quoted or continued over lines.
"""
import json
from pathlib import Path
import sys
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import workflow_reader as reader  # noqa: E402

try:
    import yaml
except ImportError:
    yaml = None

ROOT = HERE.parents[1]
WORKFLOWS = ROOT / '.github/workflows'
ACTION_FILE = ROOT / '.github/actions/cargo-deny/action.yml'
RELEASE = (WORKFLOWS / 'release.yml').read_text()

ACTION = './.github/actions/cargo-deny'
CHECK = {'uses': ACTION, 'with': {'checks': 'advisories'}}
TOOLCHAIN = {'run': 'rustup toolchain install "${RUST_TOOLCHAIN}" --profile minimal'}
RELEASE_COMMIT = '${{ needs.prepare.outputs.sha }}'
GATE_PASSED = "needs.advisories.result == 'success'"
TAG_ONLY = ("needs.prepare.outputs.publish == 'true'", "github.event_name == 'push'",
            "startsWith(github.ref, 'refs/tags/host-v')")

PUBLISH_IF = ("    if: needs.advisories.result == 'success' && needs.prepare.outputs.publish == 'true' && "
              "github.event_name == 'push' && startsWith(github.ref, 'refs/tags/host-v')\n")
TAG_BRANCH = ("needs.prepare.outputs.publish == 'true' && github.event_name == 'push' && "
              "startsWith(github.ref, 'refs/tags/host-v')")
GATE_IF = "    if: github.event_name != 'pull_request'\n"
GATE_JOB = '  advisories:\n' + RELEASE.split('  advisories:\n', 1)[1].split('  build:\n', 1)[0]
CHECK_STEP = '      - uses: ./.github/actions/cargo-deny\n        with:\n          checks: advisories\n'
RECHECK = CHECK_STEP + '      - env:\n          GH_TOKEN:'
# The publish job's own check and the step that creates the release, to the end of the file.
PUBLISH_END = RELEASE[RELEASE.rindex(CHECK_STEP):]
RELEASE_STEP = '      - env:\n          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}\n'
ASSEMBLE_END = '          cat dist/release-notes.md >> "${GITHUB_STEP_SUMMARY}"\n'

# Each entry edits release.yml so that a release could be published without a
# passing advisory check, or so that the check is no longer what it claims.
# (description, text to replace, replacement)
MUTATIONS = (
    # The publish condition, including the forms a one-line text match does not see.
    ('condition continued on the next line with an alternative that ignores the gate',
     PUBLISH_IF, PUBLISH_IF + '      || (!cancelled() && ' + TAG_BRANCH + ')\n'),
    ('condition continued on the next line with a status function',
     PUBLISH_IF, PUBLISH_IF + '      && !cancelled()\n'),
    ('condition as a folded block with an alternative',
     PUBLISH_IF, "    if: >-\n      needs.advisories.result == 'success' && " + TAG_BRANCH + '\n      || always()\n'),
    ('condition in quotes with an alternative',
     PUBLISH_IF, '    if: "' + PUBLISH_IF[8:-1] + ' || always()"\n'),
    ('alternative on the same line', PUBLISH_IF, PUBLISH_IF[:-1] + " || github.event_name == 'workflow_dispatch'\n"),
    ('alternative inside parentheses',
     PUBLISH_IF, "    if: (needs.advisories.result == 'success' || always()) && " + TAG_BRANCH + '\n'),
    ('always() added', PUBLISH_IF, PUBLISH_IF.replace('if: ', 'if: always() && ')),
    ('!cancelled() added in an expression', PUBLISH_IF, '    if: ${{ !cancelled() && ' + PUBLISH_IF[8:-1] + ' }}\n'),
    ('status function in other letter case', PUBLISH_IF, PUBLISH_IF.replace('if: ', 'if: Always() && ')),
    ('failure() added', PUBLISH_IF, PUBLISH_IF.replace('if: ', 'if: failure() && ')),
    ('gate result missing from the condition', PUBLISH_IF, PUBLISH_IF.replace(GATE_PASSED + ' && ', '')),
    ('gate result negated', PUBLISH_IF, PUBLISH_IF.replace(GATE_PASSED, "needs.advisories.result != 'success'")),
    ('gate result written as a string, which is always true',
     PUBLISH_IF, "    if: ${{ 'needs.advisories.result == ''success''' && " + TAG_BRANCH + ' }}\n'),
    ('expression markers around part of the condition, which GitHub reads as a true string',
     PUBLISH_IF, PUBLISH_IF.replace(GATE_PASSED, '${{ ' + GATE_PASSED + ' }}')),
    ('second condition key', PUBLISH_IF, PUBLISH_IF + '    if: always()\n'),
    ('tag restriction removed', PUBLISH_IF, PUBLISH_IF.replace(" && startsWith(github.ref, 'refs/tags/host-v')", '')),
    ('publish does not need the gate',
     '    needs: [prepare, assemble, advisories]\n', '    needs: [prepare, assemble]\n'),
    ('publish tolerates failure', PUBLISH_IF, PUBLISH_IF + '    continue-on-error: true\n'),
    ('publish has no condition', PUBLISH_IF, ''),
    # The gate job.
    ('gate skipped on tag pushes', GATE_IF, "    if: github.event_name == 'workflow_dispatch'\n"),
    ('gate also fails pull requests', GATE_IF, ''),
    ('gate job tolerates failure', GATE_IF, GATE_IF + '    continue-on-error: true\n'),
    ('gate step tolerates failure', GATE_JOB, GATE_JOB.replace('        with:\n          checks: advisories',
                                                             '        continue-on-error: true\n'
                                                             '        with:\n          checks: advisories')),
    ('gate runs another check', GATE_JOB, GATE_JOB.replace('checks: advisories', 'checks: bans')),
    ('gate checks the default checkout', GATE_JOB, GATE_JOB.replace('        with:\n          ref: ' + RELEASE_COMMIT
                                                                 + '\n', '')),
    ('gate job removed', GATE_JOB, ''),
    ('gate job replaced through an alias', GATE_JOB, '  advisories: *prepare\n'),
    # The check inside the publish job, immediately before the release is created.
    ('publish does not check again', RECHECK, '      - env:\n          GH_TOKEN:'),
    ('second check tolerates failure', RECHECK, RECHECK.replace('        with:\n          checks: advisories',
                                                              '        continue-on-error: true\n'
                                                              '        with:\n          checks: advisories')),
    ('second check can be skipped', RECHECK, RECHECK.replace('        with:\n          checks: advisories',
                                                           "        if: github.event_name != 'push'\n"
                                                           '        with:\n          checks: advisories')),
    ('second check runs another check', RECHECK, RECHECK.replace('checks: advisories', 'checks: bans licenses')),
    ('second check uses another action', RECHECK, RECHECK.replace(ACTION, './.github/actions/other')),
    ('second check comes after the release', PUBLISH_END, PUBLISH_END[len(CHECK_STEP):] + CHECK_STEP),
    ('release step runs after a failure', RELEASE_STEP, '      - if: always()\n        env:\n'
                                                        '          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}\n'),
    ('an earlier step creates the release', RECHECK, '      - run: gh release create "${GITHUB_REF_NAME}"\n' + RECHECK),
    ('another action runs with the write token', RECHECK, '      - uses: example/release-action@0000000\n' + RECHECK),
    ('write token left in the checkout', '          persist-credentials: false\n', ''),
    # Other ways to create a release.
    ('assemble also creates a release', ASSEMBLE_END, ASSEMBLE_END + '          gh release create x\n'),
    ('another job may write', '  assemble:\n', '  assemble:\n    permissions:\n      contents: write\n'),
    ('every job may write', '\npermissions:\n  contents: read\n', '\npermissions:\n  contents: write\n'),
    ('second publish job', '  publish:\n', '  publish:\n    name: x\n  publish:\n'),
)
# The first entry is the edit that passed an earlier test, which matched only the first line of the condition.
CONTINUED_CONDITION = MUTATIONS[0]


def mutated(description, old, new):
    if RELEASE.count(old) != 1 or old == new:
        raise ValueError('mutation does not apply exactly once: ' + description)
    return RELEASE.replace(old, new)


class HostEnvironmentJobTests(unittest.TestCase):
    def test_windows_lints_production_workspace(self):
        job = reader.read((WORKFLOWS / 'ci.yml').read_text())['jobs']['windows-staging']
        command = 'cargo clippy --locked --workspace --target x86_64-pc-windows-msvc -- -D warnings'
        self.assertTrue(any(command in step.get('run', '') for step in job['steps']))

    def test_windows_observes_startup_capture(self):
        job = reader.read((WORKFLOWS / 'ci.yml').read_text())['jobs']['windows-staging']
        command = 'cargo test --locked -p aionui-common --test startup_environment'
        self.assertTrue(any(command in step.get('run', '') for step in job['steps']))

    def test_windows_runs_the_standalone_entry_capture_regression(self):
        job = reader.read((WORKFLOWS / 'ci.yml').read_text())['jobs']['windows-staging']
        command = '--test startup_smoke standalone_entry_captures_environment_before_cli_parsing'
        self.assertTrue(any(command in step.get('run', '') for step in job['steps']))

    def test_windows_runs_host_start_environment_regression(self):
        job = reader.read((WORKFLOWS / 'ci.yml').read_text())['jobs']['windows-staging']
        self.assertTrue(job['runs-on'].startswith('windows-'))
        self.assertTrue(any(
            '--test studio_aelix_builtin_e2e host_start_consumes_the_signing_key_without_forwarding_it'
            in step.get('run', '') for step in job['steps']
        ), 'Windows CI must run the real-host environment regression')


class ReleaseGateTests(unittest.TestCase):
    def check(self, text):
        """Fail unless `text` publishes a release only after the advisory check passed, twice."""
        workflow = reader.read(text)
        self.assertEqual(workflow['permissions'], {'contents': 'read'})
        jobs = workflow['jobs']
        self.assertEqual(list(jobs), ['prepare', 'advisories', 'build', 'assemble', 'publish'])

        # The gate job checks the release commit with the shared action and cannot pass without checking.
        gate = jobs['advisories']
        self.assertEqual(sorted(gate), ['if', 'name', 'needs', 'runs-on', 'steps'])
        self.assertEqual(gate['needs'], 'prepare')
        # Only pull requests skip it, so it runs for every event that can publish.
        self.assertEqual(reader.condition_terms(gate['if']), ["github.event_name != 'pull_request'"])
        self.assertEqual(len(gate['steps']), 3)
        checkout, toolchain, check = gate['steps']
        self.assertEqual(sorted(checkout), ['uses', 'with'])
        self.assertTrue(checkout['uses'].startswith('actions/checkout@'))
        self.assertEqual(checkout['with'], {'ref': RELEASE_COMMIT})
        self.assertEqual((toolchain, check), (TOOLCHAIN, CHECK))

        # Publishing needs the gate. Its whole condition is one conjunction that names the gate's
        # result and has no status function, so nothing lets it run after a failed or skipped need.
        publish = jobs['publish']
        self.assertEqual(sorted(publish), ['if', 'name', 'needs', 'permissions', 'runs-on', 'steps'])
        self.assertEqual(publish['needs'], ['prepare', 'assemble', 'advisories'])
        terms = reader.condition_terms(publish['if'])
        for term in (GATE_PASSED,) + TAG_ONLY:
            self.assertIn(term, terms)
        self.assertEqual(publish['permissions'], {'contents': 'write'})

        # The publish job checks again as the last thing before the step that creates the release,
        # on the same commit, and no step can be skipped or allowed to fail.
        steps = publish['steps']
        for step in steps:
            self.assertNotIn('if', step)
            self.assertNotIn('continue-on-error', step)
        self.assertEqual([index for index, step in enumerate(steps) if 'gh release' in json.dumps(step)],
                         [len(steps) - 1])
        self.assertEqual(sorted(steps[-1]), ['env', 'run'])
        self.assertEqual(steps[-3:-1], [TOOLCHAIN, CHECK])
        self.assertTrue(steps[0].get('uses', '').startswith('actions/checkout@'))
        # The tooling steps must not find the write token in the checkout.
        self.assertEqual(steps[0]['with'], {'ref': RELEASE_COMMIT, 'persist-credentials': 'false'})
        self.assertEqual([step['uses'].split('@')[0] for step in steps if 'uses' in step],
                         ['actions/checkout', 'actions/download-artifact', ACTION])

        # No other job can create a release.
        for name, job in jobs.items():
            if name != 'publish':
                self.assertNotIn('permissions', job)
                self.assertNotIn('gh release', json.dumps(job))

    def test_release_is_published_only_after_the_advisory_check(self):
        self.check(RELEASE)

    def test_every_way_around_the_gate_is_refused(self):
        self.assertEqual(len({description for description, _, _ in MUTATIONS}), len(MUTATIONS))
        for description, old, new in MUTATIONS:
            with self.subTest(description):
                with self.assertRaises((AssertionError, reader.WorkflowSyntax)):
                    self.check(mutated(description, old, new))

    def test_condition_continued_over_lines_is_read_whole(self):
        condition = reader.read(mutated(*CONTINUED_CONDITION))['jobs']['publish']['if']
        self.assertEqual(condition, PUBLISH_IF[8:-1] + ' || (!cancelled() && ' + TAG_BRANCH + ')')
        with self.assertRaisesRegex(reader.WorkflowSyntax, r'\|\| in a condition'):
            reader.condition_terms(condition)

    def test_only_the_release_workflow_can_create_a_release(self):
        names = sorted(path.name for path in WORKFLOWS.iterdir())
        self.assertEqual(names, ['advisories.yml', 'ci.yml', 'release.yml'])
        for name in names:
            text = (WORKFLOWS / name).read_text()
            workflow = reader.read(text)
            grants = [workflow['permissions']] + [job['permissions'] for job in workflow['jobs'].values()
                                                  if 'permissions' in job]
            with self.subTest(name):
                for grant in grants:
                    self.assertIsInstance(grant, dict)
                if name != 'release.yml':
                    self.assertEqual([grant for grant in grants if grant.get('contents') != 'read'], [])
                    self.assertNotIn('gh release', text)

    def test_cargo_deny_action_fails_when_the_check_fails(self):
        action = reader.read(ACTION_FILE.read_text())
        self.assertEqual(action['runs']['using'], 'composite')
        install, check = action['runs']['steps']
        # Without a fallback the installer is given no GitHub token; the publish job's token can write.
        self.assertEqual(sorted(install), ['uses', 'with'])
        self.assertTrue(install['uses'].startswith('taiki-e/install-action@'))
        self.assertEqual(sorted(install['with']), ['fallback', 'tool'])
        self.assertTrue(install['with']['tool'].startswith('cargo-deny@'))
        self.assertEqual(install['with']['fallback'], 'none')
        # The check is the last command of the last step, so its exit status is the step's.
        self.assertEqual(sorted(check), ['env', 'run', 'shell'])
        self.assertEqual(check['shell'], 'bash')
        self.assertEqual(check['run'].splitlines()[-1], 'cargo deny --locked check "${checks[@]}"')
        for escape in ('|| true', 'set +e', 'exit 0'):
            self.assertNotIn(escape, check['run'])


class ConditionTests(unittest.TestCase):
    def test_conjunction_is_split_into_its_terms(self):
        self.assertEqual(reader.condition_terms("a == 'x' && startsWith(b, 'c') && (d && e)"),
                         ["a == 'x'", "startsWith(b, 'c')", '(d && e)'])
        self.assertEqual(reader.condition_terms("${{ github.ref != 'refs/heads/main' }}"),
                         ["github.ref != 'refs/heads/main'"])

    def test_operators_and_function_names_inside_strings_are_text(self):
        self.assertEqual(reader.condition_terms("contains(a, 'x || y && always()') && b == 'it''s || ok'"),
                         ["contains(a, 'x || y && always()')", "b == 'it''s || ok'"])

    def test_anything_but_a_plain_conjunction_is_refused(self):
        for condition in ('a || b', 'a && (b || c)', 'always() && a', 'a && !cancelled()', 'a && FAILURE ()',
                          'success()', '${{ a }} && b', 'a && ${{ b }}', '${{ a }} && ${{ b }}', 'a &&', '&& a', '',
                          "a == 'x", 'a && (b', 'a) && b'):
            with self.subTest(condition=condition):
                with self.assertRaises(reader.WorkflowSyntax):
                    reader.condition_terms(condition)


class ReaderTests(unittest.TestCase):
    def test_scalars_collections_and_comments(self):
        document = reader.read('\n'.join([
            '# leading comment',
            'name: Release # trailing comment',
            'empty:',
            'on:',
            '  push:',
            '    tags: ["host-v*"]',
            '  workflow_dispatch:',
            'env:',
            '  QUOTED: "600"',
            "  SINGLE: 'it''s # not a comment'",
            '  EXPRESSION: ${{ a || b }}',
            'jobs:',
            '  build:',
            '    needs: [prepare, build]',
            '    if: first &&',
            '      second',
            '      || third # comment',
            '    strategy:',
            '      matrix:',
            '        include:',
            '          - { target: linux-x64, runner: ubuntu-24.04 }',
            '    steps:',
            '      - uses: actions/checkout@0123 # v1',
            '        with:',
            '          # comment between keys',
            '          fetch-depth: 0',
            '      - name: Script',
            '        run: |',
            '          echo "a: b" # kept',
            '',
            '            indented \\',
            '        shell: bash',
            '      - plain item',
            'description: >-',
            '  folded',
            '  text',
            '',
        ]))
        self.assertEqual(document, {
            'name': 'Release', 'empty': '',
            'on': {'push': {'tags': ['host-v*']}, 'workflow_dispatch': ''},
            'env': {'QUOTED': '600', 'SINGLE': "it's # not a comment", 'EXPRESSION': '${{ a || b }}'},
            'jobs': {'build': {
                'needs': ['prepare', 'build'],
                'if': 'first && second || third',
                'strategy': {'matrix': {'include': [{'target': 'linux-x64', 'runner': 'ubuntu-24.04'}]}},
                'steps': [
                    {'uses': 'actions/checkout@0123', 'with': {'fetch-depth': '0'}},
                    {'name': 'Script', 'run': 'echo "a: b" # kept\n\n  indented \\\n', 'shell': 'bash'},
                    'plain item',
                ],
            }},
            'description': 'folded text',
        })

    def test_unmodelled_yaml_is_refused_not_guessed(self):
        for text in (
                'a: &anchor b', 'a: *alias', 'a: !!str b', 'a:\n  <<: *b', 'a: 1\na: 2', 'a:\n\tb: 1',
                'a: "two\n  lines"', "a: 'open", 'a: "back\\slash"', 'a: [b,\n  c]', 'a: [b, [c]]', 'a: {b: c}x',
                'a: |+\n  keep', 'a: >\n  folded\n\n  paragraph', 'a: |\n\n  starts blank', 'a: b: c', 'a: b:',
                'a: b # comment\n  continued', 'a: b\n\n  after a blank line', 'a:\n  b: 1\n    c: 2',
                'a:\n- b', '- a', 'a:\n  -  b', 'a:\n  - b\n  c: d', '---\na: b', 'a: b\n...', '%YAML 1.2\na: b', 'a',
                ' a: b'):
            with self.subTest(text=text):
                with self.assertRaises(reader.WorkflowSyntax):
                    reader.read(text)

    def test_reader_agrees_with_pyyaml_on_this_repository(self):
        if yaml is None:
            self.skipTest('PyYAML is not installed; the reader is compared with it where it is')

        def strings(node):
            if isinstance(node, dict):
                # YAML 1.1 reads the key `on` as a boolean.
                return {'on' if key is True else str(key): strings(item) for key, item in node.items()}
            if isinstance(node, list):
                return [strings(item) for item in node]
            if isinstance(node, bool):
                return 'true' if node else 'false'
            return '' if node is None else str(node)

        texts = [path.read_text() for path in sorted(WORKFLOWS.iterdir()) + [ACTION_FILE]]
        self.assertGreaterEqual(len(texts), 4)
        for text in texts + [mutated(*CONTINUED_CONDITION)]:
            self.assertEqual(reader.read(text), strings(yaml.safe_load(text)))


if __name__ == '__main__':
    unittest.main()
