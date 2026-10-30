"""Build, check and package one aioncore release target with a build receipt.

Adapted from Aelix Studio's scripts/studio-preview/host-recovery/build.py.
Studio built a pinned upstream archive plus a patch; this repository *is* the
patched source, so the receipt binds the git commit/tree instead.

Darwin and Windows targets build natively with the pinned Rust toolchain.
Linux targets build inside hostBuild.Dockerfile (Debian 11) on a runner of the
same CPU architecture, then the ELF GLIBC requirement is checked against the
upstream release ceiling. The output directory receives the binary, LICENSE,
NOTICE, build-receipt.json and the release archive.
"""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import time
import zipfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
POLICY = HERE / 'policy.json'
RECIPE = HERE / 'hostBuild.Dockerfile'
LABEL_PATTERN = re.compile(r'^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$')
NOTICE_FILES = ('LICENSE', 'NOTICE')

sys.path.insert(0, str(HERE))
import identity  # noqa: E402


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def git(*argv):
    return subprocess.check_output(['git', *argv], cwd=ROOT, text=True).strip()


def verify_checkout_bytes():
    """Tracked files must hold their exact git blob bytes (no core.autocrlf or filter rewrites).

    Line-ending conversion changes compiled-in text such as SQL migrations,
    whose checksums the database records, so a converted checkout is refused.
    Files that .gitattributes itself declares `eol=crlf` (PowerShell scripts)
    are the only exception.
    """
    listing = subprocess.check_output(['git', 'ls-files', '-s', '-z'], cwd=ROOT).split(b'\0')
    paths = b'\0'.join(entry.split(b'\t', 1)[1] for entry in listing if entry) + b'\0'
    attributes = subprocess.run(['git', 'check-attr', '--stdin', '-z', 'eol'], cwd=ROOT, input=paths,
                                capture_output=True, check=True).stdout.split(b'\0')
    declared_crlf = {attributes[index] for index in range(0, len(attributes) - 2, 3)
                     if attributes[index + 2] == b'crlf'}
    count = 0
    for entry in filter(None, listing):
        meta, path = entry.split(b'\t', 1)
        mode, blob = meta.split(b' ')[:2]
        if mode in (b'120000', b'160000') or path in declared_crlf:
            continue
        data = (ROOT / path.decode()).read_bytes()
        if hashlib.sha1(b'blob %d\0' % len(data) + data).hexdigest() != blob.decode():
            raise ValueError('Checkout bytes differ from git for ' + path.decode() + ' (disable core.autocrlf)')
        count += 1
    return count


def toolchain_channel():
    match = re.search(r'^channel\s*=\s*"([^"]+)"', (ROOT / 'rust-toolchain.toml').read_text(), re.M)
    if not match:
        raise ValueError('rust-toolchain.toml does not pin a channel')
    return match.group(1)


def native_target():
    architecture = {'arm64': 'arm64', 'aarch64': 'arm64', 'x86_64': 'x64', 'amd64': 'x64'}.get(platform.machine().lower())
    system = {'Darwin': 'darwin', 'Linux': 'linux', 'Windows': 'win32'}.get(platform.system())
    return str(system) + '-' + str(architecture)


def archive_name(label, rust_target):
    """Release asset name; matches Studio's `aioncore-${tag}-${rust-target}` resolver."""
    if not LABEL_PATTERN.match(label):
        raise ValueError('Unsafe release label: ' + label)
    extension = '.zip' if 'windows' in rust_target else '.tar.gz'
    return 'aioncore-' + label + '-' + rust_target + extension


def write_archive(destination, members, epoch):
    """Write a deterministic archive of (name, path, mode) members in the given order."""
    if destination.name.endswith('.zip'):
        # ZIP timestamps cannot predate 1980-01-01.
        date_time = time.gmtime(max(epoch, 315532800))[:6]
        with zipfile.ZipFile(destination, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
            for name, path, mode in members:
                info = zipfile.ZipInfo(name, date_time=date_time)
                info.compress_type = zipfile.ZIP_DEFLATED
                info.create_system = 3
                info.external_attr = (0o100000 | mode) << 16
                archive.writestr(info, Path(path).read_bytes())
        return
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode='w', format=tarfile.USTAR_FORMAT) as archive:
        for name, path, mode in members:
            data = Path(path).read_bytes()
            info = tarfile.TarInfo(name)
            info.size, info.mode, info.mtime = len(data), mode, epoch
            info.uid = info.gid = 0
            info.uname = info.gname = ''
            archive.addfile(info, io.BytesIO(data))
    with destination.open('wb') as stream:
        with gzip.GzipFile(filename='', mode='wb', fileobj=stream, mtime=0, compresslevel=9) as compressed:
            compressed.write(raw.getvalue())


def container_prefix(spec, output, target_dir, cache_dir, report):
    """Build the pinned compiler image and return the docker-run prefix for it."""
    if platform.system() not in ('Darwin', 'Linux') or not shutil.which('docker'):
        raise ValueError('Linux targets require Docker on a Linux or macOS runner')
    docker_platform = spec['container']
    recipe_sha = digest(RECIPE)
    tag = 'aelix-studio-host-build:' + recipe_sha[:20] + '-' + docker_platform.split('/')[1]
    subprocess.run(['docker', 'build', '--platform', docker_platform, '--build-arg', 'HOST_BUILD_RECIPE_SHA=' + recipe_sha,
                    '--tag', tag, '--file', str(RECIPE), str(HERE)], check=True, stdout=sys.stderr)
    image, = json.loads(subprocess.check_output(['docker', 'image', 'inspect', tag], text=True))
    if (image['Os'] != 'linux' or image['Architecture'] != docker_platform.split('/')[1]
            or image['Config']['Labels'].get('org.aelix.host-build.recipe') != recipe_sha):
        raise ValueError('Linux compiler container identity differs')
    report['buildContainer'] = {'recipeSha256': recipe_sha, 'imageId': image['Id'], 'platform': docker_platform}
    for directory in (target_dir, cache_dir / 'registry', cache_dir / 'git'):
        directory.mkdir(parents=True, exist_ok=True)
    return ['docker', 'run', '--rm', '--platform', docker_platform,
            '--user', str(os.getuid()) + ':' + str(os.getgid()),
            '--env', 'HOME=/tmp/host-build', '--env', 'LC_ALL=C', '--env', 'CARGO_INCREMENTAL=0',
            '--env', 'RUSTFLAGS=' + spec.get('rustFlags', ''),
            '--volume', str(ROOT) + ':' + str(ROOT) + ':ro',
            '--volume', str(output) + ':' + str(output) + ':ro',
            '--volume', str(target_dir) + ':' + str(target_dir) + ':rw',
            '--volume', str(cache_dir / 'registry') + ':/usr/local/cargo/registry:rw',
            '--volume', str(cache_dir / 'git') + ':/usr/local/cargo/git:rw',
            '--workdir', str(ROOT), image['Id'],
            'sh', '-c', 'mkdir -p "$HOME"; exec "$@"', 'host-build']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--target', required=True, help='Node-style target, e.g. linux-x64')
    parser.add_argument('--label', required=True, help='Release tag, or a dry-run label, used in the archive name')
    parser.add_argument('--output', type=Path, required=True, help='New output directory; existing paths are refused')
    parser.add_argument('--target-dir', type=Path, help='Cargo target directory (default: <output>/../cargo-target)')
    parser.add_argument('--cargo', default=shutil.which('cargo'), help='Cargo for native targets')
    parser.add_argument('--allow-dirty', action='store_true', help='Local experiments only; recorded in the receipt')
    args = parser.parse_args()

    policy = json.loads(POLICY.read_text())
    spec = policy['targets'].get(args.target)
    if spec is None:
        raise ValueError('Unknown target ' + args.target)
    container = 'container' in spec
    native = native_target()
    if args.target != native and not (container and native.split('-')[1] == args.target.split('-')[1]):
        raise ValueError('Build ' + args.target + ' on a matching native runner (this is ' + native + ')')
    status = git('status', '--porcelain', '--untracked-files=no')
    dirty = status != ''
    if dirty and not args.allow_dirty:
        raise ValueError('Refusing to build a release target from a modified checkout')
    # git status normalizes line endings, so a converted checkout still looks clean.
    checked_files = None if dirty else verify_checkout_bytes()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    target_dir = (args.target_dir or output.parent / 'cargo-target').resolve()
    rust_target, binary_name = spec['rustTarget'], policy['binary'] + ('.exe' if args.target.startswith('win32-') else '')
    channel = toolchain_channel()
    commit = git('rev-parse', 'HEAD')
    epoch = int(git('show', '-s', '--format=%ct', 'HEAD'))
    report = {
        'schema': 'aelix-studio-host.build-receipt/1', 'status': 'failed',
        'repository': policy['repository'], 'commit': commit, 'tree': git('rev-parse', 'HEAD^{tree}'),
        'dirty': dirty, 'checkoutFilesVerified': checked_files, 'upstream': policy['upstream'], 'label': args.label,
        'target': args.target, 'rustTarget': rust_target, 'binary': binary_name,
        'rustToolchain': channel, 'rustFlags': spec.get('rustFlags', ''), 'profile': 'release',
        'policySha256': digest(POLICY), 'buildRecipeSha256': digest(Path(__file__)),
        'cargoLockSha256': digest(ROOT / 'Cargo.lock'), 'licenseSha256': digest(ROOT / 'LICENSE'),
    }
    receipt = output / 'build-receipt.json'
    try:
        prefix = container_prefix(spec, output, target_dir, target_dir.parent / 'container-cargo', report) if container else []
        if not container and not args.cargo:
            raise ValueError('Cargo with the pinned toolchain is required')
        cargo = '/usr/local/cargo/bin/cargo' if container else str(Path(args.cargo).absolute())
        rustc = '/usr/local/cargo/bin/rustc' if container else str(Path(cargo).with_name('rustc.exe' if os.name == 'nt' else 'rustc'))
        environment = dict(os.environ, RUSTUP_TOOLCHAIN=channel, RUSTFLAGS=spec.get('rustFlags', ''), CARGO_INCREMENTAL='0')
        environment.pop('CARGO_ENCODED_RUSTFLAGS', None)

        def capture(argv):
            return subprocess.check_output([*prefix, *argv], cwd=ROOT, env=environment, text=True).strip()

        report['cargoVersion'] = capture([cargo, '--version'])
        report['rustcVersionVerbose'] = capture([rustc, '--version', '--verbose'])
        if not (report['cargoVersion'].startswith('cargo ' + channel + ' ')
                and report['rustcVersionVerbose'].startswith('rustc ' + channel + ' ')):
            raise ValueError('Compiler differs from the pinned Rust toolchain ' + channel)
        if container:
            report['buildContainer']['osRelease'] = capture(['cat', '/etc/os-release'])
        argv = [cargo, 'build', '--locked', '--release', '-p', policy['package'], '--bin', policy['binary'],
                '--target', rust_target, '--target-dir', str(target_dir)]
        report['buildArgv'] = argv
        subprocess.run([*prefix, *argv], cwd=ROOT, env=environment, check=True)
        binary = output / binary_name
        shutil.copy2(target_dir / rust_target / 'release' / binary_name, binary)
        if container:
            checked = capture(['bash', str(ROOT / 'scripts/check-glibc-baseline.sh'), str(binary), spec['glibcMax']])
            report['glibcCeiling'] = spec['glibcMax']
            report['maximumGlibcVersionNeeded'] = re.search(r'Required GLIBC (GLIBC_[0-9.]+) is within', checked).group(1)
        for name in NOTICE_FILES:
            shutil.copy2(ROOT / name, output / name)
        report['noticeSha256'] = digest(output / 'NOTICE')
        report['binarySha256'] = digest(binary)
        report['binaryCodeSha256'] = (identity.macho_code_identity(binary.read_bytes())
                                      if args.target.startswith('darwin-') else report['binarySha256'])
        report['identityRecipeSha256'] = digest(HERE / 'identity.py')
        report['version'] = subprocess.check_output([str(binary), '--version'], text=True).strip() \
            if args.target == native else None
        if git('status', '--porcelain', '--untracked-files=no') != status:
            raise ValueError('The checkout changed during the build')
        report['archive'] = archive_name(args.label, rust_target)
        report['status'] = 'built'
    finally:
        receipt.write_text(json.dumps(report, indent=2) + '\n')
    members = [(binary_name, output / binary_name, 0o755)]
    members += [(name, output / name, 0o644) for name in (*NOTICE_FILES, 'build-receipt.json')]
    write_archive(output / report['archive'], members, epoch)
    print(json.dumps({'status': report['status'], 'archive': report['archive'], 'binarySha256': report['binarySha256']}))


if __name__ == '__main__':
    main()
