"""Tracking Issue decisions, and diagnostics captured from cargo-deny 0.20.2.

fixtures/advisories-failed.jsonl is the stderr of
`cargo deny --frozen --format json check advisories` on main 396eb446 with the
RustSec database of 2026-11-16: one unsound advisory and two yanked warnings.
fixtures/metadata-failed.jsonl is the same command when `cargo metadata` fails.
"""
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import advisory_issue as tracking  # noqa: E402

FIXTURES = HERE / 'fixtures'
RUSTIX = {'id': 'RUSTSEC-2026-0486', 'kind': 'unsound', 'package': 'rustix', 'version': '1.1.4'}
SQLITE = {'id': 'RUSTSEC-2026-0459', 'kind': 'vulnerability', 'package': 'libsqlite3-sys', 'version': '0.30.1'}
CONTEXT = {'branch': 'main', 'commit': '396eb4460123456789abcdef0123456789abcdef', 'checked': '2026-11-16 00:17 UTC',
           'run': 'https://github.com/example/host/actions/runs/7'}


def report(*advisories, unidentified=0):
    return {'advisories': list(advisories), 'unidentified': unidentified}


def diagnostic(advisory, severity='error', code='vulnerability', graphs=None):
    fields = {'severity': severity, 'code': code, 'graphs': graphs or []}
    if advisory is not None:
        fields['advisory'] = advisory
    return json.dumps({'type': 'diagnostic', 'fields': fields})


class FakeGitHub:
    """Issues held in memory, with the calls made on them."""

    def __init__(self, issues=()):
        self.stored = [dict(issue) for issue in issues]
        self.calls = []

    def issues(self):
        return [dict(issue) for issue in self.stored]

    def find(self, number):
        return next(issue for issue in self.stored if issue['number'] == number)

    def create(self, title, body):
        number = max([issue['number'] for issue in self.stored] + [40]) + 1
        self.stored.append({'number': number, 'state': 'open', 'author': tracking.AUTHOR, 'body': body,
                            'title': title, 'comments': []})
        self.calls.append(('create', number))
        return number

    def edit(self, number, changes):
        self.find(number).update(changes)
        self.calls.append(('edit', number, sorted(changes)))

    def comment(self, number, body):
        self.find(number).setdefault('comments', []).append(body)
        self.calls.append(('comment', number))


class DiagnosticsTest(unittest.TestCase):
    def test_captured_failure_names_the_advisory_and_ignores_warnings(self):
        text = (FIXTURES / 'advisories-failed.jsonl').read_text(encoding='utf-8')
        self.assertEqual(text.count('"code":"yanked"'), 2)
        self.assertEqual(tracking.parse_diagnostics(text), report(RUSTIX))

    def test_captured_metadata_failure_is_unidentified(self):
        text = (FIXTURES / 'metadata-failed.jsonl').read_text(encoding='utf-8')
        self.assertEqual(tracking.parse_diagnostics(text), report(unidentified=2))

    def test_advisories_are_sorted_and_reported_once(self):
        graph = [{'Krate': {'name': 'rustix', 'version': '1.1.4'}, 'parents': []}]
        rustix = {'id': RUSTIX['id'], 'package': 'rustix', 'title': 'ignored'}
        sqlite = {'id': SQLITE['id'], 'package': 'libsqlite3-sys'}
        text = '\n'.join([
            diagnostic(rustix, code='unsound', graphs=graph),
            diagnostic(rustix, code='unsound', graphs=graph),
            diagnostic(sqlite, graphs=[{'Krate': {'name': 'libsqlite3-sys', 'version': '0.30.1'}}]),
            diagnostic(rustix, severity='warning'),
            diagnostic(None, severity='warning', code='yanked'),
            json.dumps({'type': 'summary', 'fields': {'advisories': {'errors': 2}}}),
            json.dumps({'type': 'log', 'fields': {'level': 'WARN', 'message': 'not an error'}}),
            '',
        ])
        self.assertEqual(tracking.parse_diagnostics(text), report(SQLITE, RUSTIX))

    def test_errors_without_a_rustsec_id_are_counted_not_copied(self):
        text = '\n'.join([
            diagnostic(None),
            diagnostic({'id': 'RUSTSEC-2026-0486 @someone', 'package': 'rustix'}),
            diagnostic({'id': ['RUSTSEC-2026-0486']}),
            json.dumps({'type': 'log', 'fields': {'level': 'ERROR', 'message': 'failed to fetch advisory database'}}),
            json.dumps({'type': 'diagnostic', 'fields': 'unexpected'}),
            json.dumps(['unexpected']),
            'error: not json',
        ])
        self.assertEqual(tracking.parse_diagnostics(text), report(unidentified=7))

    def test_text_that_is_not_a_plain_name_is_dropped(self):
        hostile = {'id': RUSTIX['id'], 'package': 'rustix](https://example.com) @someone'}
        graph = [{'Krate': {'name': 'rustix', 'version': '1.1.4 | <b>x</b>'}}]
        parsed = tracking.parse_diagnostics(diagnostic(hostile, code='Unsound [x]', graphs=graph))
        self.assertEqual(parsed, report({'id': RUSTIX['id'], 'kind': '', 'package': '', 'version': ''}))
        named = tracking.parse_diagnostics(diagnostic({'id': RUSTIX['id'], 'package': 'rustix'}, graphs=graph))
        self.assertEqual(named['advisories'][0]['version'], '')

    def test_report_survives_the_job_boundary_and_rejects_anything_else(self):
        line = json.dumps(report(SQLITE, RUSTIX, unidentified=1))
        self.assertEqual(tracking.load_report(line), report(SQLITE, RUSTIX, unidentified=1))
        hostile = {'advisories': [{'id': 'RUSTSEC-2026-0486\n@someone'}, 'text', {'id': RUSTIX['id'], 'kind': 'A B',
                                                                                 'package': '@someone', 'version': 1}],
                   'unidentified': 'many'}
        self.assertEqual(tracking.load_report(json.dumps(hostile)),
                         report({'id': RUSTIX['id'], 'kind': '', 'package': '', 'version': ''}))
        for text in ('', '   ', 'not json', '[]', '{"advisories": "x"}', 'null'):
            with self.subTest(text=text):
                self.assertEqual(tracking.load_report(text), report())

    def test_report_command_prints_one_github_output_line(self):
        output = io.StringIO()
        argv = ['advisory_issue.py', 'report', '--input', str(FIXTURES / 'advisories-failed.jsonl')]
        with mock.patch.object(sys, 'argv', argv), contextlib.redirect_stdout(output):
            tracking.main()
        self.assertEqual(output.getvalue(), 'report={"advisories":[{"id":"RUSTSEC-2026-0486","kind":"unsound",'
                                            '"package":"rustix","version":"1.1.4"}],"unidentified":0}\n')
        output = io.StringIO()
        with mock.patch.object(sys, 'argv', argv[:3] + ['/nonexistent/diagnostics.jsonl']), \
                contextlib.redirect_stdout(output):
            tracking.main()
        self.assertEqual(output.getvalue(), 'report={"advisories":[],"unidentified":0}\n')


class BodyTest(unittest.TestCase):
    def test_failing_body_lists_advisories_and_records_their_ids(self):
        body = tracking.render_body(False, report(SQLITE, RUSTIX), CONTEXT)
        self.assertTrue(body.startswith(
            tracking.MARKER + '\n<!-- advisory-ids: RUSTSEC-2026-0459, RUSTSEC-2026-0486 -->\n'))
        self.assertIn('`cargo deny --locked check advisories` fails on `main`.', body)
        self.assertIn('| Status | failing |', body)
        self.assertIn('| Checked | 2026-11-16 00:17 UTC at `396eb4460123` |', body)
        self.assertIn('| Run | https://github.com/example/host/actions/runs/7 |', body)
        self.assertIn(
            '| [RUSTSEC-2026-0486](https://rustsec.org/advisories/RUSTSEC-2026-0486) | unsound | rustix 1.1.4 |', body)
        self.assertNotIn('names no RustSec advisory', body)
        self.assertEqual(tracking.recorded_ids(body), ['RUSTSEC-2026-0459', 'RUSTSEC-2026-0486'])

    def test_failure_without_an_identified_advisory_points_to_the_log(self):
        body = tracking.render_body(False, report(), CONTEXT)
        self.assertIn('The run failed with an error that names no RustSec advisory', body)
        self.assertNotIn('| Advisory |', body)
        self.assertEqual(tracking.recorded_ids(body), [])
        both = tracking.render_body(False, report(RUSTIX, unidentified=2), CONTEXT)
        self.assertIn('The run failed with an error that names no RustSec advisory', both)
        self.assertIn('| [RUSTSEC-2026-0486]', both)

    def test_passing_body_keeps_the_marker_and_no_ids(self):
        body = tracking.render_body(True, report(RUSTIX), CONTEXT)
        self.assertTrue(body.startswith(tracking.MARKER + '\n'))
        self.assertIn('| Status | passing |', body)
        self.assertNotIn('RUSTSEC', body)
        self.assertEqual(tracking.recorded_ids(body), [])

    def test_recorded_ids_ignore_edited_text(self):
        self.assertEqual(tracking.recorded_ids(None), [])
        self.assertEqual(tracking.recorded_ids('no marker'), [])
        self.assertEqual(tracking.recorded_ids('<!-- advisory-ids: RUSTSEC-2026-0486, BOGUS, RUSTSEC-2026-0001 -->'),
                         ['RUSTSEC-2026-0001', 'RUSTSEC-2026-0486'])


class SyncTest(unittest.TestCase):
    def sync(self, github, passing, *advisories, **options):
        return tracking.sync(github, passing, report(*advisories, **options), CONTEXT)

    def test_weekly_results_keep_one_issue_through_its_whole_life(self):
        github = FakeGitHub([{'number': 30, 'state': 'open', 'author': 'handochan', 'body': 'unrelated'}])

        self.assertEqual(self.sync(github, True), 'check passes; no open tracking Issue')
        self.assertEqual(github.calls, [])

        self.assertEqual(self.sync(github, False, RUSTIX), 'created #41')
        tracker = github.find(41)
        self.assertEqual(tracker['title'], '[Host] RustSec advisory check fails on main')
        self.assertEqual(github.calls, [('create', 41)])

        # The same failure a week later rewrites the text and notifies nobody.
        self.assertEqual(self.sync(github, False, RUSTIX), 'updated #41')
        self.assertEqual(github.calls[1:], [('edit', 41, ['body'])])
        self.assertEqual(tracker['comments'], [])

        self.assertEqual(self.sync(github, False, SQLITE, RUSTIX), 'updated #41 with a comment')
        self.assertEqual(github.calls[2:], [('comment', 41), ('edit', 41, ['body'])])
        self.assertIn('New: RUSTSEC-2026-0459. Now reported: RUSTSEC-2026-0459, RUSTSEC-2026-0486.',
                      tracker['comments'][-1])

        self.assertEqual(self.sync(github, False, SQLITE), 'updated #41 with a comment')
        self.assertIn('No longer reported: RUSTSEC-2026-0486. Now reported: RUSTSEC-2026-0459.',
                      tracker['comments'][-1])

        self.assertEqual(self.sync(github, True), 'closed #41')
        self.assertEqual(github.calls[-2:], [('comment', 41), ('edit', 41, ['body', 'state', 'state_reason'])])
        self.assertEqual((tracker['state'], tracker['state_reason']), ('closed', 'completed'))
        self.assertIn('| Status | passing |', tracker['body'])

        self.assertEqual(self.sync(github, True), 'check passes; no open tracking Issue')

        # The next failure reopens the same Issue, even for the advisory it already reported.
        self.assertEqual(self.sync(github, False, SQLITE), 'reopened #41')
        self.assertEqual(github.calls[-2:], [('comment', 41), ('edit', 41, ['body', 'state'])])
        self.assertEqual(tracker['state'], 'open')
        self.assertIn('fails again', tracker['comments'][-1])
        self.assertIn('Reported: RUSTSEC-2026-0459.', tracker['comments'][-1])

        self.assertEqual([call for call in github.calls if call[0] == 'create'], [('create', 41)])
        self.assertEqual(len(github.stored), 2)
        self.assertEqual(github.find(30)['body'], 'unrelated')

    def test_issue_closed_by_hand_is_reopened_not_duplicated(self):
        github = FakeGitHub()
        self.sync(github, False, RUSTIX)
        github.find(41)['state'] = 'closed'
        self.assertEqual(self.sync(github, False, RUSTIX), 'reopened #41')
        self.assertEqual(len(github.stored), 1)

    def test_failure_without_ids_is_tracked_and_not_repeated_as_comments(self):
        github = FakeGitHub()
        self.assertEqual(self.sync(github, False, unidentified=2), 'created #41')
        self.assertEqual(self.sync(github, False), 'updated #41')
        self.assertEqual(github.find(41)['comments'], [])
        self.assertEqual(self.sync(github, False, RUSTIX), 'updated #41 with a comment')

    def test_only_the_bots_marked_issue_is_the_tracker(self):
        marked = tracking.render_body(False, report(RUSTIX), CONTEXT)
        github = FakeGitHub([
            {'number': 5, 'state': 'open', 'author': 'someone', 'body': marked},
            {'number': 6, 'state': 'open', 'author': tracking.AUTHOR, 'body': 'another automated Issue'},
            {'number': 7, 'state': 'open', 'author': tracking.AUTHOR, 'body': None},
        ])
        self.assertEqual(self.sync(github, True), 'check passes; no open tracking Issue')
        self.assertEqual(self.sync(github, False, RUSTIX), 'created #41')
        self.assertEqual([github.find(number)['state'] for number in (5, 6, 7)], ['open'] * 3)
        self.assertEqual(github.find(5)['body'], marked)

    def test_open_tracker_wins_over_closed_ones(self):
        marked = tracking.render_body(False, report(RUSTIX), CONTEXT)
        issues = [{'number': number, 'state': state, 'author': tracking.AUTHOR, 'body': marked}
                  for number, state in ((9, 'closed'), (12, 'open'), (15, 'open'), (20, 'closed'))]
        self.assertEqual(tracking.find_tracker(issues)['number'], 12)
        self.assertEqual(tracking.find_tracker([issue for issue in issues if issue['state'] == 'closed'])['number'], 20)
        self.assertIsNone(tracking.find_tracker([]))


class CommandTest(unittest.TestCase):
    """The `gh api` calls `sync` makes, with the GitHub CLI replaced by a recorder."""

    def run_sync(self, issues, *arguments):
        """Run `sync` against the listed Issue rows; return (stdout, recorded calls)."""
        calls = []

        def run(command, input=None, **_options):
            calls.append({'arguments': command[1:], 'payload': None if input is None else json.loads(input)})
            listing = ''.join(json.dumps(row) + '\n' for row in issues)
            return subprocess.CompletedProcess(command, 0, stdout=listing if 'GET' in command else '41\n', stderr='')

        output = io.StringIO()
        argv = ['advisory_issue.py', 'sync', '--repository', 'example/host', '--branch', 'main',
                '--commit', CONTEXT['commit'], '--run', CONTEXT['run'], *arguments]
        with mock.patch.object(sys, 'argv', argv), mock.patch.object(tracking.subprocess, 'run', run), \
                contextlib.redirect_stdout(output), contextlib.redirect_stderr(io.StringIO()):
            tracking.main()
        return output.getvalue(), calls

    def test_failure_lists_the_bots_issues_then_creates_one(self):
        output, calls = self.run_sync([], '--result', 'failure', '--report', json.dumps(report(RUSTIX)))
        self.assertEqual(output, 'created #41\n')
        self.assertEqual(calls[0]['arguments'], [
            'api', '--method', 'GET', 'repos/example/host/issues', '--paginate', '-f', 'state=all',
            '-f', 'creator=github-actions[bot]', '-f', 'per_page=100', '--jq',
            '.[] | select(.pull_request == null) | [.number, .state, .user.login, .body // ""] | @json'])
        self.assertEqual(calls[1]['arguments'],
                         ['api', '--method', 'POST', 'repos/example/host/issues', '--jq', '.number', '--input', '-'])
        self.assertEqual(calls[1]['payload']['title'], '[Host] RustSec advisory check fails on main')
        self.assertIn('RUSTSEC-2026-0486', calls[1]['payload']['body'])
        self.assertEqual(len(calls), 2)

    def test_pass_closes_the_listed_tracker(self):
        body = tracking.render_body(False, report(RUSTIX), CONTEXT)
        issues = [[12, 'open', 'github-actions[bot]', body], [13, 'open', 'someone', body]]
        output, calls = self.run_sync(issues, '--result', 'success')
        self.assertEqual(output, 'closed #12\n')
        self.assertEqual([call['arguments'][2:4] for call in calls[1:]],
                         [['POST', 'repos/example/host/issues/12/comments'], ['PATCH', 'repos/example/host/issues/12']])
        self.assertEqual(calls[2]['payload']['state'], 'closed')
        self.assertEqual(calls[2]['payload']['state_reason'], 'completed')

    def test_dry_run_reads_but_does_not_write(self):
        output, calls = self.run_sync([], '--result', 'failure', '--dry-run')
        self.assertEqual(output, 'created #dry-run\n')
        self.assertEqual(len(calls), 1)
        self.assertIn('GET', calls[0]['arguments'])

    def test_failed_listing_stops_before_any_write(self):
        calls = []

        def run(command, **_options):
            calls.append(command)
            raise subprocess.CalledProcessError(1, command, stderr='HTTP 502')

        argv = ['advisory_issue.py', 'sync', '--repository', 'example/host', '--branch', 'main',
                '--commit', CONTEXT['commit'], '--run', CONTEXT['run'], '--result', 'failure']
        with mock.patch.object(sys, 'argv', argv), mock.patch.object(tracking.subprocess, 'run', run), \
                self.assertRaises(SystemExit) as raised:
            tracking.main()
        self.assertEqual(raised.exception.code, 'gh api --method GET repos/example/host/issues failed: HTTP 502')
        self.assertEqual(len(calls), 1)

    def test_missing_context_outside_actions_is_refused(self):
        names = ('GITHUB_REPOSITORY', 'GITHUB_REF_NAME', 'GITHUB_SHA', 'GITHUB_SERVER_URL', 'GITHUB_RUN_ID')
        environment = {name: value for name, value in os.environ.items() if name not in names}
        with mock.patch.dict(os.environ, environment, clear=True), \
                mock.patch.object(sys, 'argv', ['advisory_issue.py', 'sync', '--result', 'failure']), \
                contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as raised:
            tracking.main()
        self.assertEqual(raised.exception.code, 2)


if __name__ == '__main__':
    unittest.main()
