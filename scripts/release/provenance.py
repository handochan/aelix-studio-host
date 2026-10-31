"""Release gate and provenance assembly for the aioncore release workflow.

`prepare` validates the ref being built and prints `label=...` and
`publish=...` lines for $GITHUB_OUTPUT. Only a `host-v<version>-aelix.<n>` tag
on a commit already on origin/main can publish; every other event is a
dry run labelled `dryrun-<commit>`.

`assemble` checks every target's build receipt, archive members and smoke
report against each other and the checked-out commit, then writes the
release assets: the archives, aioncore-checksums.txt (sha256sum format) and
aioncore-<label>-provenance.json naming the upstream base and our commit.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import zipfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
TAG_PATTERN = re.compile(r'^host-v(\d+\.\d+\.\d+)-aelix\.([1-9]\d*)$')
CHECKSUMS = 'aioncore-checksums.txt'
MEMBERS = ('LICENSE', 'NOTICE', 'build-receipt.json')


def digest_bytes(data):
    return hashlib.sha256(data).hexdigest()


def git(*argv, check=True):
    result = subprocess.run(['git', *argv], cwd=ROOT, capture_output=True, text=True)
    if check and result.returncode != 0:
        raise RuntimeError('git ' + ' '.join(argv) + ' failed: ' + result.stderr.strip())
    return result


def is_ancestor(ancestor, descendant):
    return git('merge-base', '--is-ancestor', ancestor, descendant, check=False).returncode == 0


def workspace_version():
    text = (ROOT / 'Cargo.toml').read_text()
    section = text.split('[workspace.package]', 1)[1].split('\n[', 1)[0]
    return re.search(r'^version\s*=\s*"([^"]+)"', section, re.M).group(1)


def load_policy():
    return json.loads((HERE / 'policy.json').read_text())


def check_upstream(policy):
    """The recorded upstream base must be in our history, match its tag, and be the workspace's version."""
    upstream = policy['upstream']
    if upstream['tag'] != 'v' + workspace_version():
        raise ValueError('policy.json upstream tag ' + upstream['tag'] + ' is stale for workspace version '
                         + workspace_version())
    if not is_ancestor(upstream['commit'], 'HEAD'):
        raise ValueError('Upstream base ' + upstream['commit'] + ' is not an ancestor of HEAD')
    tagged = git('rev-parse', '--verify', '--quiet', upstream['tag'] + '^{commit}', check=False)
    if tagged.returncode == 0 and tagged.stdout.strip() != upstream['commit']:
        raise ValueError('Upstream tag ' + upstream['tag'] + ' does not point at the recorded base commit')
    return int(git('rev-list', '--count', upstream['commit'] + '..HEAD').stdout.strip())


def release_label(event, ref, commit, main_ref):
    """Return (label, publish) for this workflow event, refusing anything ambiguous."""
    if event == 'push' and ref.startswith('refs/tags/'):
        tag = ref[len('refs/tags/'):]
        match = TAG_PATTERN.match(tag)
        if not match:
            raise ValueError('Release tags must look like host-v<version>-aelix.<n>: ' + tag)
        if match.group(1) != workspace_version():
            raise ValueError('Tag version ' + match.group(1) + ' differs from the Cargo workspace version '
                             + workspace_version())
        if not is_ancestor(commit, main_ref):
            raise ValueError('Release tag ' + tag + ' must point at a commit on ' + main_ref)
        return tag, True
    if ref.startswith('refs/tags/'):
        raise ValueError('Tag refs publish only through a tag push')
    return 'dryrun-' + commit[:12], False


def read_member(archive_path, name):
    if archive_path.name.endswith('.zip'):
        with zipfile.ZipFile(archive_path) as archive:
            return archive.read(name)
    with tarfile.open(archive_path) as archive:
        return archive.extractfile(name).read()


def member_names(archive_path):
    if archive_path.name.endswith('.zip'):
        with zipfile.ZipFile(archive_path) as archive:
            return archive.namelist()
    with tarfile.open(archive_path) as archive:
        members = archive.getmembers()
        if any(not member.isfile() for member in members):
            raise ValueError('Archive contains a non-regular member: ' + archive_path.name)
        return [member.name for member in members]


def assemble(label, source, output, commit, environment):
    policy = load_policy()
    commits_since_upstream = check_upstream(policy)
    output.mkdir(parents=True, exist_ok=False)
    targets, receipts = {}, []
    for name, spec in policy['targets'].items():
        directory = source / ('release-' + name)
        receipt = json.loads((directory / 'build-receipt.json').read_text())
        smoke = json.loads((directory / 'smoke.json').read_text())
        expected = {'status': 'built', 'commit': commit, 'dirty': False, 'label': label, 'target': name,
                    'rustTarget': spec['rustTarget'], 'upstream': policy['upstream'],
                    'binary': policy['binary'] + ('.exe' if name.startswith('win32-') else '')}
        for key, value in expected.items():
            if receipt.get(key) != value:
                raise ValueError(name + ': build receipt ' + key + ' is ' + repr(receipt.get(key)))
        archive = directory / receipt['archive']
        binary = receipt['binary']
        if member_names(archive) != [binary, *MEMBERS]:
            raise ValueError(name + ': unexpected archive members ' + repr(member_names(archive)))
        if digest_bytes(read_member(archive, binary)) != receipt['binarySha256']:
            raise ValueError(name + ': archived binary differs from its receipt')
        if json.loads(read_member(archive, 'build-receipt.json')) != receipt:
            raise ValueError(name + ': archived receipt differs')
        for notice in ('LICENSE', 'NOTICE'):
            if read_member(archive, notice) != (ROOT / notice).read_bytes():
                raise ValueError(name + ': archived ' + notice + ' differs from the checkout')
        if smoke.get('status') != 'passed' or smoke.get('binarySha256') != receipt['binarySha256']:
            raise ValueError(name + ': host smoke did not pass for this binary')
        expected_version = policy['binary'] + ' ' + workspace_version()
        if receipt.get('version') != expected_version or smoke.get('version') != expected_version:
            raise ValueError(name + ': binary version is not ' + expected_version)
        receipts.append(receipt)
        shutil.copy2(archive, output / archive.name)
        targets[name] = {
            'rustTarget': spec['rustTarget'], 'archive': archive.name,
            'archiveSha256': digest_bytes(archive.read_bytes()), 'binary': binary,
            'binarySha256': receipt['binarySha256'], 'binaryCodeSha256': receipt['binaryCodeSha256'],
            'rustFlags': receipt['rustFlags'], 'cargoVersion': receipt['cargoVersion'],
            'rustcVersionVerbose': receipt['rustcVersionVerbose'],
            'buildContainer': receipt.get('buildContainer'),
            'maximumGlibcVersionNeeded': receipt.get('maximumGlibcVersionNeeded'),
            'glibcCeiling': receipt.get('glibcCeiling'),
            'smoke': {key: smoke.get(key) for key in ('status', 'checks', 'version')},
        }
    for key in ('tree', 'cargoLockSha256', 'licenseSha256', 'noticeSha256', 'rustToolchain', 'policySha256',
                'buildRecipeSha256', 'checkoutFilesVerified'):
        if len({receipt[key] for receipt in receipts}) != 1:
            raise ValueError('Targets disagree on ' + key)
    first = receipts[0]
    server = environment.get('GITHUB_SERVER_URL', 'https://github.com')
    repository = environment.get('GITHUB_REPOSITORY', policy['repository'])
    run_id = environment.get('GITHUB_RUN_ID')
    provenance = {
        'schema': 'aelix-studio-host.release-provenance/1',
        'label': label,
        'kind': 'release' if TAG_PATTERN.match(label) else 'dry-run',
        'repository': policy['repository'],
        'commit': commit,
        'tree': first['tree'],
        'workspaceVersion': workspace_version(),
        'upstream': dict(policy['upstream'], commitsSinceUpstream=commits_since_upstream),
        'build': {
            'workflow': environment.get('GITHUB_WORKFLOW_REF'),
            'event': environment.get('GITHUB_EVENT_NAME'),
            'runId': run_id,
            'runAttempt': environment.get('GITHUB_RUN_ATTEMPT'),
            'runUrl': run_id and server + '/' + repository + '/actions/runs/' + run_id,
        },
        'rustToolchain': first['rustToolchain'],
        'cargoLockSha256': first['cargoLockSha256'],
        'licenseSha256': first['licenseSha256'],
        'noticeSha256': first['noticeSha256'],
        'targets': targets,
    }
    provenance_name = 'aioncore-' + label + '-provenance.json'
    (output / provenance_name).write_text(json.dumps(provenance, indent=2) + '\n')
    names = sorted([target['archive'] for target in targets.values()] + [provenance_name])
    lines = [digest_bytes((output / name).read_bytes()) + '  ' + name for name in names]
    (output / CHECKSUMS).write_text('\n'.join(lines) + '\n')
    return provenance


def notes(provenance):
    upstream = provenance['upstream']
    rows = '\n'.join('| ' + name + ' | `' + target['archive'] + '` | `' + target['archiveSha256'] + '` |'
                     for name, target in provenance['targets'].items())
    return f"""Aelix Studio host `{provenance['label']}`: a modified build of
[{upstream['repository']}](https://github.com/{upstream['repository']}) for Aelix Studio.

- Release commit: `{provenance['commit']}`
- Upstream base: `{upstream['tag']}` (`{upstream['commit']}`), {upstream['commitsSinceUpstream']} commits since
- `aelix-studio-host --version` reports the upstream workspace version `{provenance['workspaceVersion']}`
- Rust {provenance['rustToolchain']}; every archive passed the host smoke on its own runner

| Target | Archive | SHA-256 |
| --- | --- | --- |
{rows}

Verify with `sha256sum -c {CHECKSUMS}`. `aioncore-{provenance['label']}-provenance.json`
records the build receipts. Each archive contains `LICENSE` and `NOTICE`; see the
repository README for the upstream license notice and the list of modifications.
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    prepare = commands.add_parser('prepare')
    prepare.add_argument('--event', required=True)
    prepare.add_argument('--ref', required=True)
    prepare.add_argument('--main-ref', default='origin/main')
    combine = commands.add_parser('assemble')
    combine.add_argument('--label', required=True)
    combine.add_argument('--input', type=Path, required=True)
    combine.add_argument('--output', type=Path, required=True, help='New directory for the release assets only')
    combine.add_argument('--notes', type=Path, required=True, help='Release notes path, outside --output')
    args = parser.parse_args()
    commit = git('rev-parse', 'HEAD').stdout.strip()
    if args.command == 'prepare':
        check_upstream(load_policy())
        label, publish = release_label(args.event, args.ref, commit, args.main_ref)
        print('label=' + label)
        print('publish=' + ('true' if publish else 'false'))
        return
    provenance = assemble(args.label, args.input.resolve(), args.output.resolve(), commit, os.environ)
    args.notes.write_text(notes(provenance))
    print(json.dumps({'label': provenance['label'], 'kind': provenance['kind'],
                      'targets': sorted(provenance['targets'])}))


if __name__ == '__main__':
    main()
