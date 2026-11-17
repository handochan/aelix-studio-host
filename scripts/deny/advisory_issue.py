"""Record the scheduled RustSec advisory check in one tracking Issue (host #30).

`report` reads the diagnostics written by
`cargo deny --format json check advisories` and prints a `report=<json>` line
for $GITHUB_OUTPUT naming the advisories that failed the check.

`sync` records a check result in the repository's single tracking Issue. A
failure creates the Issue, or reopens and rewrites the existing one; a pass
closes it. The Issue is recognised by the marker in its body and by its
author, the Actions bot, so a failing run never opens a second one.
"""
import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import subprocess
import sys

MARKER = '<!-- aelix-studio-host:advisory-check -->'
AUTHOR = 'github-actions[bot]'
TITLE = '[Host] RustSec advisory check fails on {branch}'
RECORDED_IDS = re.compile(r'<!-- advisory-ids:([A-Z0-9, -]*)-->')
# Everything copied from the advisory database or the lockfile into an Issue
# must match one of these; free text such as advisory titles is never copied.
ADVISORY_ID = re.compile(r'^RUSTSEC-\d{4}-\d{4}$')
KIND = re.compile(r'^[a-z][a-z-]{0,39}$')
PACKAGE = re.compile(r'^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$')
VERSION = re.compile(r'^[0-9][0-9A-Za-z.+-]{0,63}$')


def clean(value, pattern):
    return value if isinstance(value, str) and pattern.match(value) else ''


def parse_diagnostics(text):
    """Return {'advisories': [...], 'unidentified': n} for cargo-deny's JSON diagnostics.

    `unidentified` counts error records that name no RustSec advisory: other
    diagnostics, error logs and lines that are not JSON.
    """
    found, unidentified = {}, 0
    for line in text.splitlines():
        if not line.strip():
            continue
        try:
            record = json.loads(line)
            fields = record['fields']
            kind = record['type']
        except (ValueError, KeyError, TypeError):
            unidentified += 1
            continue
        if not isinstance(fields, dict):
            unidentified += 1
        elif kind == 'diagnostic' and fields.get('severity') == 'error':
            advisory = fields.get('advisory') if isinstance(fields.get('advisory'), dict) else {}
            identifier = clean(advisory.get('id'), ADVISORY_ID)
            if not identifier:
                unidentified += 1
                continue
            package = clean(advisory.get('package'), PACKAGE)
            version = ''
            for graph in fields.get('graphs') or []:
                krate = graph.get('Krate') if isinstance(graph, dict) else None
                if isinstance(krate, dict) and package and krate.get('name') == package:
                    version = clean(krate.get('version'), VERSION)
                    break
            entry = {'id': identifier, 'kind': clean(fields.get('code'), KIND), 'package': package, 'version': version}
            found[(identifier, package, version)] = entry
        elif kind == 'log' and str(fields.get('level', '')).lower() == 'error':
            unidentified += 1
    return {'advisories': [found[key] for key in sorted(found)], 'unidentified': unidentified}


def load_report(text):
    """Validate a report that crossed a job boundary; anything malformed is dropped."""
    try:
        data = json.loads(text) if text.strip() else {}
    except ValueError:
        data = {}
    entries = data.get('advisories') if isinstance(data, dict) else None
    advisories = {}
    for entry in entries if isinstance(entries, list) else []:
        if not isinstance(entry, dict) or not clean(entry.get('id'), ADVISORY_ID):
            continue
        item = {'id': entry['id'], 'kind': clean(entry.get('kind'), KIND),
                'package': clean(entry.get('package'), PACKAGE), 'version': clean(entry.get('version'), VERSION)}
        advisories[(item['id'], item['package'], item['version'])] = item
    unidentified = data.get('unidentified') if isinstance(data, dict) else 0
    return {'advisories': [advisories[key] for key in sorted(advisories)],
            'unidentified': unidentified if isinstance(unidentified, int) and unidentified > 0 else 0}


def advisory_ids(report):
    return sorted({entry['id'] for entry in report['advisories']})


def recorded_ids(body):
    match = RECORDED_IDS.search(body or '')
    return sorted({item for item in re.split(r'[,\s]+', match.group(1)) if ADVISORY_ID.match(item)}) if match else []


def render_body(passing, report, context):
    ids = [] if passing else advisory_ids(report)
    command = '`cargo deny --locked check advisories`'
    lines = [
        MARKER,
        '<!-- advisory-ids: ' + ', '.join(ids) + ' -->',
        command + (' passes on `%s`.' if passing else ' fails on `%s`.') % context['branch'],
        '',
        '| | |',
        '| --- | --- |',
        '| Status | ' + ('passing' if passing else 'failing') + ' |',
        '| Checked | %s at `%s` |' % (context['checked'], context['commit'][:12]),
        '| Run | %s |' % context['run'],
        '',
    ]
    if not passing:
        if report['advisories']:
            lines += ['| Advisory | Kind | Crate |', '| --- | --- | --- |']
            for entry in report['advisories']:
                crate = ' '.join(part for part in (entry['package'], entry['version']) if part)
                lines.append('| [%s](https://rustsec.org/advisories/%s) | %s | %s |'
                             % (entry['id'], entry['id'], entry['kind'] or '-', crate or '-'))
            lines.append('')
        if report['unidentified'] or not report['advisories']:
            lines += ['The run failed with an error that names no RustSec advisory, for example because the advisory '
                      'database could not be fetched or an earlier step failed. Read the run log.', '']
        lines += ['Resolve each advisory by updating or removing the dependency. A `deny.toml` entry needs the '
                  "repository owner's decision. A release tag on a commit that fails this check is not published. "
                  'README.md "Dependency checks" describes which runs check advisories.', '']
    lines += ['The Advisories workflow maintains this Issue: each run on `%s` rewrites this text, a failing run '
              'reopens the Issue and a passing run closes it. Keep the first line of this text (an HTML comment); '
              'without it the workflow opens a new Issue.' % context['branch']]
    return '\n'.join(lines) + '\n'


def find_tracker(issues):
    """Pick the tracking Issue: the oldest open one, otherwise the newest closed one."""
    own = [issue for issue in issues if issue['author'] == AUTHOR and MARKER in (issue['body'] or '')]
    still_open = sorted((issue for issue in own if issue['state'] == 'open'), key=lambda issue: issue['number'])
    closed = sorted((issue for issue in own if issue['state'] != 'open'), key=lambda issue: -issue['number'])
    return (still_open or closed or [None])[0]


def sync(github, passing, report, context):
    """Apply one check result to the tracking Issue and return what was done."""
    tracker = find_tracker(github.issues())
    body = render_body(passing, report, context)
    where = '%s at `%s` (%s)' % (context['checked'], context['commit'][:12], context['run'])
    if passing:
        if tracker is None or tracker['state'] != 'open':
            return 'check passes; no open tracking Issue'
        # Comment before the edit that records the new state: a run that stops
        # between the two repeats the comment next time instead of losing it.
        github.comment(tracker['number'], 'The advisory check passes: ' + where + '. Closing.')
        github.edit(tracker['number'], {'body': body, 'state': 'closed', 'state_reason': 'completed'})
        return 'closed #%s' % tracker['number']
    ids = advisory_ids(report)
    listed = ', '.join(ids) if ids else 'no RustSec advisory ID reported'
    if tracker is None:
        number = github.create(TITLE.format(branch=context['branch']), body)
        return 'created #%s' % number
    if tracker['state'] != 'open':
        github.comment(tracker['number'], 'The advisory check fails again: ' + where + '. Reported: ' + listed + '.')
        github.edit(tracker['number'], {'body': body, 'state': 'open'})
        return 'reopened #%s' % tracker['number']
    previous = recorded_ids(tracker['body'])
    if ids != previous:
        added = [item for item in ids if item not in previous]
        resolved = [item for item in previous if item not in ids]
        changes = ['New: ' + ', '.join(added) + '.' if added else '',
                   'No longer reported: ' + ', '.join(resolved) + '.' if resolved else '']
        github.comment(tracker['number'], 'The reported advisories changed: ' + where + '. '
                       + ' '.join(change for change in changes if change) + ' Now reported: ' + listed + '.')
    github.edit(tracker['number'], {'body': body})
    return 'updated #%s' % tracker['number'] + ('' if ids == previous else ' with a comment')


class GitHub:
    """The Issue calls `sync` needs, through the GitHub CLI and its GH_TOKEN."""

    def __init__(self, repository, dry_run=False):
        self.repository, self.dry_run = repository, dry_run

    def api(self, arguments, payload=None):
        command = ['gh', 'api', *arguments] + (['--input', '-'] if payload is not None else [])
        data = None if payload is None else json.dumps(payload)
        try:
            return subprocess.run(command, input=data, check=True, capture_output=True, text=True).stdout
        except subprocess.CalledProcessError as error:
            # Stop before any further call: a failed listing must not lead to a second Issue.
            sys.exit('gh api %s failed: %s' % (' '.join(arguments[:3]), (error.stderr or '').strip()))

    def write(self, method, path, payload, query='.number'):
        if self.dry_run:
            print('dry run: %s %s %s' % (method, path, json.dumps(payload, indent=2)), file=sys.stderr)
            return 'dry-run'
        return self.api(['--method', method, path, '--jq', query], payload).strip()

    def issues(self):
        """Every Issue the Actions bot opened here, in any state."""
        query = '.[] | select(.pull_request == null) | [.number, .state, .user.login, .body // ""] | @json'
        output = self.api(['--method', 'GET', 'repos/%s/issues' % self.repository, '--paginate', '-f', 'state=all',
                           '-f', 'creator=' + AUTHOR, '-f', 'per_page=100', '--jq', query])
        rows = [json.loads(line) for line in output.splitlines() if line.strip()]
        return [{'number': number, 'state': state, 'author': author, 'body': body}
                for number, state, author, body in rows]

    def create(self, title, body):
        return self.write('POST', 'repos/%s/issues' % self.repository, {'title': title, 'body': body})

    def edit(self, number, changes):
        return self.write('PATCH', 'repos/%s/issues/%s' % (self.repository, number), changes)

    def comment(self, number, body):
        return self.write('POST', 'repos/%s/issues/%s/comments' % (self.repository, number), {'body': body}, '.id')


def main():
    environment = os.environ
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    report = commands.add_parser('report')
    report.add_argument('--input', required=True, type=Path, help='cargo-deny JSON diagnostics')
    synchronise = commands.add_parser('sync')
    synchronise.add_argument('--result', required=True, choices=('success', 'failure'))
    synchronise.add_argument('--report', default='', help='the report line value from `report`')
    synchronise.add_argument('--repository', default=environment.get('GITHUB_REPOSITORY', ''))
    synchronise.add_argument('--branch', default=environment.get('GITHUB_REF_NAME', ''))
    synchronise.add_argument('--commit', default=environment.get('GITHUB_SHA', ''))
    run = [environment.get(name, '') for name in ('GITHUB_SERVER_URL', 'GITHUB_REPOSITORY', 'GITHUB_RUN_ID')]
    synchronise.add_argument('--run', default='%s/%s/actions/runs/%s' % tuple(run) if all(run) else '',
                             help='URL of the workflow run')
    synchronise.add_argument('--dry-run', action='store_true', help='read Issues but print the writes instead')
    arguments = parser.parse_args()

    if arguments.command == 'report':
        text = arguments.input.read_text(encoding='utf-8', errors='replace') if arguments.input.is_file() else ''
        print('report=' + json.dumps(parse_diagnostics(text), separators=(',', ':'), sort_keys=True))
        return
    for name in ('repository', 'branch', 'commit', 'run'):
        if not getattr(arguments, name):
            parser.error('--' + name + ' is required outside GitHub Actions')
    context = {'branch': arguments.branch, 'commit': arguments.commit, 'run': arguments.run,
               'checked': datetime.now(timezone.utc).strftime('%Y-%m-%d %H:%M UTC')}
    print(sync(GitHub(arguments.repository, arguments.dry_run), arguments.result == 'success',
               load_report(arguments.report), context))


if __name__ == '__main__':
    main()
