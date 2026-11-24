"""Generate or check the exact vendored aion-mcp diff against its pinned tag."""

import argparse
import io
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile


ROOT = Path(__file__).resolve().parents[2]
VENDOR = ROOT / 'vendor' / 'aion-mcp'
DIFF = VENDOR / 'UPSTREAM.diff'
UPSTREAM = 'https://github.com/iOfficeAI/aionrs.git'
TAG = 'v0.2.11'
COMMIT = '8e61a90329fa9f67c4fdf7e97fe02c24dba33f75'


def git(cwd, *args):
    return subprocess.check_output(['git', '-C', cwd, *args])


def generate_diff(upstream, staging):
    if upstream is None:
        upstream = staging / 'upstream.git'
        subprocess.run(['git', 'clone', '--bare', '--depth=1', '--single-branch',
                        '--branch', TAG, UPSTREAM, upstream], check=True)
        if git(upstream, 'rev-parse', f'refs/tags/{TAG}^{{commit}}').decode().strip() != COMMIT:
            raise ValueError('The aionrs source tag no longer matches the pinned commit')

    before = staging / f'aion-mcp-{TAG}'
    after = staging / 'aion-mcp-host'
    before.mkdir()
    after.mkdir()
    prefix = 'crates/aion-mcp/'
    archive = git(upstream, 'archive', COMMIT, prefix)
    with tarfile.open(fileobj=io.BytesIO(archive)) as source:
        for member in source.getmembers():
            if member.isfile():
                relative = Path(member.name).relative_to(prefix)
                destination = before / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(source.extractfile(member).read())
                destination.chmod(member.mode)

    files = git(ROOT, 'ls-files', '-z', '--cached', '--others', '--exclude-standard', '--', VENDOR)
    for name in set(files.split(b'\0')) - {b''}:
        local = ROOT / name.decode()
        if local == DIFF or not local.is_file():
            continue
        destination = after / local.relative_to(VENDOR)
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(local, destination)

    result = subprocess.run(
        ['git', '-c', 'core.quotePath=true', 'diff', '--no-index', '--no-ext-diff',
         '--no-textconv', '--no-color', '--no-renames', '--full-index', '--binary',
         '--unified=0', '--diff-algorithm=myers', '--no-indent-heuristic', '--src-prefix=a/', '--dst-prefix=b/', before.name, after.name],
        cwd=staging, stdout=subprocess.PIPE,
    )
    if result.returncode != 1:
        raise ValueError(f'Expected a vendored-source diff, got git exit {result.returncode}')
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true', help='Verify the committed diff without rewriting it')
    parser.add_argument('--upstream-checkout', type=Path, help='Reuse a local checkout containing the pinned commit')
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as directory:
        diff = generate_diff(args.upstream_checkout, Path(directory))
    if args.check:
        if not DIFF.is_file() or DIFF.read_bytes() != diff:
            parser.exit(1, 'vendor/aion-mcp/UPSTREAM.diff is stale. Regenerate it using this script.\n')
    else:
        DIFF.write_bytes(diff)


if __name__ == '__main__':
    main()
