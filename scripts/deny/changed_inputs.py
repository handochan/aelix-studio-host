"""Decide whether a CI run must also check RustSec advisories (host #30).

`cargo deny check advisories` compares the dependency graph with the RustSec
database, which changes outside this repository, so ordinary pushes and pull
requests do not run it. A change to one of the files in INPUTS can itself
bring an advisory into the checked graph, so CI runs the check when the
change under test touches any of them.

Prints `changed=true|false` and `reason=...` lines for $GITHUB_OUTPUT. When
the change under test cannot be determined the answer is `changed=true`.
"""
import argparse
import os
import re
import subprocess

# Git pathspecs of the repository files that decide the advisory result.
INPUTS = (
    'Cargo.lock',  # the resolved packages the database is matched against
    'deny.toml',  # ignore list, checked targets and advisory settings
    ':(glob)**/Cargo.toml',  # features and targets select the checked graph without changing Cargo.lock
    '.github/actions/cargo-deny',  # the cargo-deny version and invocation
)
COMMIT = re.compile(r'^[0-9a-f]{40}([0-9a-f]{24})?$')
BRANCH = re.compile(r'^[A-Za-z0-9][A-Za-z0-9._/-]*$')
SHOWN = 5


class Undetermined(Exception):
    """The change under test cannot be compared; the caller checks advisories."""


def git(arguments, cwd):
    try:
        return subprocess.run(['git', *arguments], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError) as error:
        raise Undetermined('git ' + ' '.join(arguments[:2]) + ' failed') from error


def commit(value, name):
    """Accept only a full commit id, so event data never reaches git as an option or a ref name."""
    if not value or not COMMIT.match(value) or set(value) == {'0'}:
        raise Undetermined('no usable ' + name + ' commit')
    return value


def comparison(event, ref, default_branch, pr_base, pr_head, before, cwd=None):
    """Return (base, head, description) of the change this run tests."""
    if not BRANCH.match(default_branch):
        raise Undetermined('unusable default branch name')
    if event == 'pull_request':
        base, head = commit(pr_base, 'pull request base'), commit(pr_head, 'pull request head')
        return git(['merge-base', base, head], cwd), head, 'the pull request'
    if event == 'push' and ref == 'refs/heads/' + default_branch:
        # A push to the default branch is compared with the commit it replaced.
        base = commit(before, 'previous ' + default_branch)
        git(['cat-file', '-e', base + '^{commit}'], cwd)
        return base, 'HEAD', 'this push to ' + default_branch
    # Other branches and manual runs: everything not yet on the default branch,
    # however the pushes were split.
    upstream = 'refs/remotes/origin/' + default_branch
    return git(['merge-base', upstream, 'HEAD'], cwd), 'HEAD', 'commits not on origin/' + default_branch


def decide(event, ref, default_branch='main', pr_base='', pr_head='', before='', cwd=None):
    """Return (changed, reason)."""
    try:
        base, head, scope = comparison(event, ref, default_branch, pr_base, pr_head, before, cwd)
        names = git(['diff', '--name-only', '--no-renames', base, head, '--', *INPUTS], cwd).splitlines()
    except Undetermined as error:
        return True, 'cannot determine the change under test (' + str(error) + '); checking advisories'
    if not names:
        return False, 'no dependency input changed in ' + scope
    listed = ', '.join(names[:SHOWN]) + (' and %d more' % (len(names) - SHOWN) if len(names) > SHOWN else '')
    return True, listed + ' changed in ' + scope


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--event', default=os.environ.get('GITHUB_EVENT_NAME', ''))
    parser.add_argument('--ref', default=os.environ.get('GITHUB_REF', ''))
    parser.add_argument('--default-branch', default='main')
    parser.add_argument('--pr-base', default='', help='base commit of the pull request event')
    parser.add_argument('--pr-head', default='', help='head commit of the pull request event')
    parser.add_argument('--before', default='', help='previous commit of the push event')
    arguments = parser.parse_args()
    changed, reason = decide(arguments.event, arguments.ref, arguments.default_branch, arguments.pr_base,
                             arguments.pr_head, arguments.before)
    print('changed=' + ('true' if changed else 'false'))
    print('reason=' + ' '.join(reason.split()))


if __name__ == '__main__':
    main()
