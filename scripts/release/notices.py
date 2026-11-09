"""Target-specific third-party notices for the default-feature aionui-app build.

cargo-about resolves SPDX choices. Original licensing files, copyright comment
blocks, git repository-root notices and pinned upstream supplements are retained
independently of its file detector/canonical-text fallback. Native source has a
separate, exact-hash policy approved in host #20.
"""
import argparse
import base64
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))
from license_policy import ROOT, HERE, canonical, sha256, license_policy, check_crate
from native_sources import check_native, copyright_comments, is_notice, source_files

FILES = ('THIRD-PARTY-NOTICES.txt', 'third-party-licenses.json')
POLICIES = ('native-sources.json', 'license-sources.json', 'license-tools.json')


def load(name):
    return json.loads((HERE / name).read_text(encoding='utf-8'))


def tool_spec(rust_target):
    lock = load('license-tools.json')
    host = rust_target.replace('-unknown-linux-gnu', '-unknown-linux-musl')
    name = 'cargo-about-' + lock['version'] + '-' + host + '.tar.gz'
    return lock['version'], next(x for x in lock['archives'] if x['name'] == name)


def install_tool(rust_target, directory):
    version, spec = tool_spec(rust_target)
    directory.mkdir(parents=True, exist_ok=True)
    archive = directory / spec['name']
    url = 'https://github.com/EmbarkStudios/cargo-about/releases/download/' + version + '/' + spec['name']
    with urllib.request.urlopen(url, timeout=120) as response:
        data = response.read()
    if sha256(data) != spec['sha256']:
        raise ValueError('cargo-about download checksum mismatch')
    archive.write_bytes(data)
    return extract_tool(archive, rust_target, directory)


def extract_tool(archive, rust_target, directory):
    _, spec = tool_spec(rust_target)
    if sha256(archive.read_bytes()) != spec['sha256']:
        raise ValueError('cargo-about archive checksum mismatch')
    binary = 'cargo-about.exe' if 'windows' in rust_target else 'cargo-about'
    with tarfile.open(archive) as packed:
        member = packed.getmember(spec['name'][:-7] + '/' + binary)
        if not member.isfile():
            raise ValueError('cargo-about binary is not a regular archive member')
        data = packed.extractfile(member).read()
    destination = directory / binary
    destination.write_bytes(data)
    destination.chmod(0o755)
    return destination


def checked_tool_run(argv, *, cwd=ROOT):
    result = subprocess.run([str(x) for x in argv], cwd=cwd, capture_output=True, text=True,
                            encoding='utf-8', env=dict(os.environ, CARGO_TERM_COLOR='never'))
    # cargo-about 0.8.4 can log a parse error and still return 0. Keep the
    # complete diagnostics as evidence, and reject every error diagnostic.
    diagnostics = re.sub(r'\x1b\[[0-9;]*m', '', result.stderr)
    if result.returncode or re.search(r'(?im)\bERROR\b|failed to parse|error:', diagnostics):
        raise ValueError('License tool failed (exit ' + str(result.returncode) + '):\n' + diagnostics)
    return result.stdout, diagnostics


def make_config(policy, native_policy, packages):
    lines = ['accepted = ' + json.dumps(policy['allow']), 'no-clearly-defined = true',
             'ignore-dev-dependencies = true', 'ignore-build-dependencies = false',
             'private = { ignore = true }']
    for exception in policy['exceptions']:
        # cargo-about's crate-specific accepts are name-only. Post-validation
        # below enforces the exact deny.toml version before accepting output.
        lines += ['[' + exception['name'] + ']', 'accepted = ' + json.dumps(exception['allow'])]
    for entry in native_policy['packages']:
        clarification = entry.get('wrapperClarification')
        if not clarification:
            continue
        for package in packages.values():
            if package['name'] != entry['crate']:
                continue
            if package['version'] != entry['version']:
                raise ValueError('Native wrapper version changed: ' + package['name'])
            spec = clarification['file']
            path = Path(package['manifest_path']).parent / spec['path']
            if sha256(path.read_bytes()) != spec['sha256']:
                raise ValueError('Native wrapper license changed: ' + package['name'])
            # Only the wrapper metadata is clarified. All bundled C conditions
            # and originals remain mandatory in native-sources.json.
            lines += ['[' + entry['crate'] + '.clarify]',
                      'license = ' + json.dumps(clarification['license']),
                      '[[' + entry['crate'] + '.clarify.files]]',
                      'path = ' + json.dumps(spec['path']),
                      'license = ' + json.dumps(clarification['fileLicense']),
                      'checksum = ' + json.dumps(spec['sha256'])]
    return '\n'.join(lines) + '\n'


def original(path, data, *, kind='source-file'):
    if not data:
        raise ValueError('Empty original license/notice: ' + path)
    # Base64 preserves the precise original bytes, including non-UTF8 credits.
    return {'path': path, 'kind': kind, 'sha256': sha256(data), 'bytesBase64': base64.b64encode(data).decode('ascii')}


def decode_original(item):
    data = base64.b64decode(item['bytesBase64'], validate=True)
    if not data or sha256(data) != item['sha256']:
        raise ValueError('Original notice bytes do not match hash: ' + item['path'])
    return data.decode('utf-8', errors='replace')


def check_required_originals(package, originals, supplements):
    for required in supplements.get('apacheAudit', {}).get('requiredNotices', []):
        if (required['crate'], required['version']) == (package['name'], package['version']):
            if not any(item['path'] == 'upstream/' + required['path'] and item['sha256'] == required['sha256']
                       for item in originals):
                raise ValueError('Missing audited upstream NOTICE: ' + package['name'])
    if (package.get('source') or '').startswith('git+'):
        pins = [p for p in supplements.get('gitRootLicenses', []) if p['source'] == package['source']]
        if not pins:
            raise ValueError('Git dependency root license has not been audited: ' + package['name'])
        for pin in pins:
            if not any(item['path'] == 'repository-root/' + pin['path'] and item['sha256'] == pin['sha256']
                       for item in originals):
                raise ValueError('Missing or changed git root license: ' + package['name'])


def package_originals(package, supplements):
    root = Path(package['manifest_path']).parent
    if 'Apache-2.0' in (package.get('license') or '') and not (package.get('source') or '').startswith('git+'):
        if package['name'] + '@' + package['version'] not in supplements['apacheAudit']['packageVersions']:
            raise ValueError('Apache NOTICE audit is missing for package/version: ' + package['name'])
    originals = []
    seen_comments = set()
    for path in source_files(root):
        relative = path.relative_to(root)
        if is_notice(relative):
            originals.append(original(relative.as_posix(), path.read_bytes()))
        elif path.suffix.lower() in ('.rs', '.c', '.h', '.cc', '.cpp', '.s', '.asm'):
            # Latin-1 is a byte-preserving intermediate for comment matching;
            # original bytes are kept even for legacy non-UTF8 source credits.
            text = path.read_bytes().decode('latin-1')
            for index, block in enumerate(copyright_comments(text)):
                if block in seen_comments:
                    continue
                seen_comments.add(block)
                originals.append(original(relative.as_posix() + '#copyright-' + str(index + 1),
                                          block.encode('latin-1'), kind='copyright-comment'))
    if (package.get('source') or '').startswith('git+'):
        directory = root
        while not (directory / '.git').exists() and directory != directory.parent:
            directory = directory.parent
        if not (directory / '.git').exists():
            raise ValueError('Cannot locate git dependency repository root')
        found = False
        # Include all repository-root licensing files; nested package files
        # were already scanned above. Do not guess from workspace metadata.
        for path in sorted(directory.iterdir()):
            if path.is_file() and is_notice(Path(path.name)):
                originals.append(original('repository-root/' + path.name, path.read_bytes()))
                found = True
        if not found:
            raise ValueError('Git dependency has no original root license: ' + package['name'])
    for entry in supplements['files']:
        if entry['crate'] != package['name'] or entry['version'] != package['version']:
            continue
        vcs = json.loads((root / '.cargo_vcs_info.json').read_text())
        if vcs['git']['sha1'] != entry['commit']:
            raise ValueError('Supplemental license source revision differs: ' + package['name'])
        data = (HERE / 'licenses' / entry['sha256']).read_bytes()
        if sha256(data) != entry['sha256']:
            raise ValueError('Supplemental license checksum differs')
        item = original('upstream/' + entry['path'], data, kind='pinned-upstream-original')
        item['source'] = {k: entry[k] for k in ('repository', 'commit', 'path')}
        originals.append(item)
    if not any(item['kind'] != 'copyright-comment' for item in originals):
        raise ValueError('Missing original license file: ' + package['name'] + ' ' + package['version'])
    check_required_originals(package, originals, supplements)
    return originals


def render(bundle):
    parts = ['THIRD-PARTY LICENSES AND NOTICES', 'Target: ' + bundle['rustTarget'],
             'Package: aionui-app; default features; target-filtered graph including build tools.',
             'Original alternative/test-source notices are retained conservatively.', '']
    for entry in bundle['crates']:
        parts += ['=' * 72, entry['name'] + ' ' + entry['version'],
                  'Source: ' + entry['source'], 'License metadata: ' + entry['license'],
                  'Selected: ' + ', '.join(entry['selectedLicenses']),
                  'Authors (package metadata): ' + '; '.join(entry['authors'])]
        for license_id in entry['selectedLicenses']:
            if not entry['licenseTexts'].get(license_id):
                raise ValueError('Selected license has no text')
            for text in entry['licenseTexts'][license_id]:
                parts += ['--- ' + license_id + ' ---', text]
        for item in entry['originals']:
            parts += ['--- Original ' + item['path'] + ' (SHA-256 ' + item['sha256'] + ') ---', decode_original(item)]
    for entry in bundle['nativeSources']:
        parts += ['=' * 72, 'NATIVE SOURCE: ' + entry['crate'] + ' ' + entry['version'],
                  'Actual Cargo link result: ' + entry['linkage'] + ' ' + repr(entry['linkedLibraries'])]
        for component in entry['components']:
            parts += [component['component'] + ' ' + component['version'], component['licenseConditions']]
        for item in entry['notices']:
            parts += ['--- Original native notice ' + item['path'] + ' ---', item['text']]
    for package in bundle.get('privatePackages', []):
        parts += ['=' * 72, 'PRIVATE DEPENDENCY: ' + package['name'] + ' ' + package['version'],
                  'Metadata is ignored by deny.toml private policy; original notices follow.']
        for item in package['originals']:
            parts += ['--- Original ' + item['path'] + ' ---', decode_original(item)]
    return ('\n\n'.join(parts) + '\n').encode('utf-8')


def generate(rust_target, cargo, archive, output, build_log=None):
    work = output / 'license-work'
    work.mkdir(parents=True, exist_ok=True)
    about = extract_tool(archive, rust_target, work)
    version, spec = tool_spec(rust_target)
    actual_version, _ = checked_tool_run([about, '--version'])
    if actual_version.strip() != 'cargo-about ' + version:
        raise ValueError('cargo-about version differs from pin')
    metadata_text, metadata_diagnostics = checked_tool_run([cargo, 'metadata', '--locked', '--offline',
                                                          '--format-version', '1', '--filter-platform', rust_target])
    metadata = json.loads(metadata_text)
    packages = {p['id']: p for p in metadata['packages']}
    native_policy = load('native-sources.json')
    policy = license_policy()
    config = make_config(policy, native_policy, packages)
    config_path = work / 'about.toml'
    config_path.write_bytes(config.encode('utf-8'))
    argv = [str(about), '-c', 'never', 'generate', '--locked', '--offline', '--fail', '--format', 'json',
            '--config', str(config_path), '--manifest-path', str(ROOT / 'crates/aionui-app/Cargo.toml'),
            '--target', rust_target]
    raw, diagnostics = checked_tool_run(argv)
    resolved = json.loads(raw)
    (work / 'cargo-about.json').write_bytes(raw.encode('utf-8'))
    (work / 'cargo-about.stderr').write_bytes(diagnostics.encode('utf-8'))
    selected, texts = {}, {}
    for entry in resolved['licenses']:
        for user in entry['used_by']:
            key = user['crate']['id']
            selected.setdefault(key, set()).add(entry['id'])
            texts.setdefault(key, {}).setdefault(entry['id'], []).append(entry['text'])
    events = []
    if build_log:
        messages = [json.loads(line) for line in build_log.read_text(encoding='utf-8').splitlines() if line.strip()]
        if not any(m.get('reason') == 'build-finished' and m.get('success') is True for m in messages):
            raise ValueError('Native audit needs a successful Cargo build result')
        events = [m for m in messages if m.get('reason') == 'build-script-executed']
    # Private workspace crates are covered by the host LICENSE/NOTICE. They
    # must not introduce unaudited native sources, even though cargo-about
    # correctly excludes their absent license metadata.
    for package in packages.values():
        if package.get('publish') == []:
            check_native(package, Path(package['manifest_path']).parent, native_policy)
    supplements = load('license-sources.json')
    bundle = {'schema': 'aelix-studio-host.third-party-notices/1', 'rustTarget': rust_target,
              'scope': 'aionui-app default features; target graph; includes build dependencies',
              'denySha256': sha256((ROOT / 'deny.toml').read_bytes()),
              'policySha256': {p: sha256((HERE / p).read_bytes()) for p in POLICIES},
              'crates': [], 'privatePackages': [], 'nativeSources': [], 'apacheNoticeFiles': [], 'missingNotices': []}
    for package in packages.values():
        if package.get('publish') == [] and package.get('source'):
            bundle['privatePackages'].append({k: package[k] for k in ('name', 'version', 'source', 'license')})
            bundle['privatePackages'][-1]['originals'] = package_originals(package, supplements)
    included = set()
    for entry in resolved['crates']:
        package = entry['package']
        key = package['id']
        check_crate(package, selected.get(key, set()), policy)
        originals = package_originals(package, supplements)
        item = {k: package[k] for k in ('name', 'version', 'source', 'authors', 'license')}
        item.update(selectedLicenses=sorted(selected[key]),
                    licenseTexts={k: sorted(set(v)) for k, v in sorted(texts[key].items())}, originals=originals)
        bundle['crates'].append(item)
        included.add(key)
        for original_item in originals:
            if re.match(r'(?i)^notice(?:$|[._-])', Path(original_item['path']).name):
                bundle['apacheNoticeFiles'].append({'crate': package['name'], 'path': original_item['path'], 'sha256': original_item['sha256']})
        native = check_native(package, Path(package['manifest_path']).parent, native_policy,
                              [m for m in events if m['package_id'] == key])
        if native:
            bundle['nativeSources'].append(native)
    # Do not let a newly introduced build script hide native linkage behind
    # metadata without a links key or without any checked-in C source files.
    native_names = {p['crate'] for p in bundle['nativeSources']}
    for event in events:
        if event.get('linked_libs') or event.get('linked_paths'):
            package = packages[event['package_id']]
            if package['name'] not in native_names:
                raise ValueError('Unlisted native linker output: ' + package['name'])
        package = packages[event['package_id']]
        if package['id'] not in included and package.get('source') and package.get('publish') != []:
            raise ValueError('Built third-party package missing from license graph: ' + package['name'])
    if build_log:
        for message in messages:
            if message.get('reason') != 'compiler-artifact':
                continue
            package = packages[message['package_id']]
            if package['id'] not in included and package.get('source') and package.get('publish') != []:
                raise ValueError('Compiled third-party package missing from license graph: ' + package['name'])
    bundle['crates'].sort(key=lambda x: (x['name'], x['version']))
    bundle['nativeSources'].sort(key=lambda x: x['crate'])
    json_bytes = canonical(bundle)
    text_bytes = render(bundle)
    for name, data in zip(FILES, (text_bytes, json_bytes)):
        (output / name).write_bytes(data)
    result = {
        'status': 'passed' if build_log else 'source-audit-only', 'rustTarget': rust_target,
        'tool': {'name': 'cargo-about', 'version': version, 'archive': spec['name'],
                 'archiveSha256': spec['sha256'], 'binarySha256': sha256(about.read_bytes())},
        'argv': argv, 'exitCode': 0, 'errorDiagnostics': 0, 'stderrSha256': sha256(diagnostics.encode()),
        'configSha256': sha256(config.encode()), 'rawResultSha256': sha256(raw.encode()),
        'generatorSha256': sha256(Path(__file__).read_bytes()),
        'helperSha256': {name: sha256((HERE / name).read_bytes())
                         for name in ('license_policy.py', 'native_sources.py')},
        'denySha256': bundle['denySha256'], 'policySha256': bundle['policySha256'],
        'crateCount': len(bundle['crates']), 'nativePackageCount': len(bundle['nativeSources']),
        'apacheNoticeFiles': bundle['apacheNoticeFiles'], 'missingNotices': [],
        'files': {name: sha256(data) for name, data in zip(FILES, (text_bytes, json_bytes))},
    }
    (output / 'third-party-result.json').write_bytes(canonical(result))
    return result


def validate_bundle(text_bytes, json_bytes, result, rust_target):
    bundle = json.loads(json_bytes)
    if result.get('status') != 'passed' or result.get('exitCode') != 0 or result.get('errorDiagnostics') != 0:
        raise ValueError('Third-party notice generation did not pass')
    if bundle.get('rustTarget') != rust_target or result.get('rustTarget') != rust_target:
        raise ValueError('Third-party notice target differs')
    version, spec = tool_spec(rust_target)
    if result['tool']['version'] != version or result['tool']['archiveSha256'] != spec['sha256']:
        raise ValueError('Third-party license tool differs from pin')
    if result.get('generatorSha256') != sha256(Path(__file__).read_bytes()):
        raise ValueError('Third-party notice generator differs')
    for name in ('license_policy.py', 'native_sources.py'):
        if result.get('helperSha256', {}).get(name) != sha256((HERE / name).read_bytes()):
            raise ValueError('Third-party notice helper differs: ' + name)
    for name in POLICIES:
        expected = sha256((HERE / name).read_bytes())
        if bundle['policySha256'].get(name) != expected or result['policySha256'].get(name) != expected:
            raise ValueError('Third-party notice policy differs: ' + name)
    expected = sha256((ROOT / 'deny.toml').read_bytes())
    if bundle['denySha256'] != expected or result['denySha256'] != expected:
        raise ValueError('Third-party notices use a different deny.toml')
    if result.get('missingNotices') != [] or bundle.get('missingNotices') != [] or not bundle['crates']:
        raise ValueError('Missing third-party notices')
    for name, data in zip(FILES, (text_bytes, json_bytes)):
        if result['files'].get(name) != sha256(data):
            raise ValueError('Third-party notice checksum differs: ' + name)
    if render(bundle) != text_bytes:
        raise ValueError('Readable notices do not match inventory')
    policy = license_policy()
    supplements = load('license-sources.json')
    if result['crateCount'] != len(bundle['crates']) or result['nativePackageCount'] != len(bundle['nativeSources']):
        raise ValueError('Third-party notice counts differ')
    if len({(p['name'], p['version']) for p in bundle['crates']}) != len(bundle['crates']):
        raise ValueError('Duplicate third-party package')
    for package in bundle['crates']:
        check_crate(package, package['selectedLicenses'], policy)
        if set(package['licenseTexts']) != set(package['selectedLicenses']) or any(
                not texts or not all(text.strip() for text in texts) for texts in package['licenseTexts'].values()):
            raise ValueError('Selected license has no text')
        if not package['originals']:
            raise ValueError('Missing original crate notice')
        for item in package['originals']:
            decode_original(item)
        check_required_originals(package, package['originals'], supplements)
    for package in bundle.get('privatePackages', []):
        if not package['originals']:
            raise ValueError('Private third-party package is missing its original notices')
        for item in package['originals']:
            decode_original(item)
        check_required_originals(package, package['originals'], supplements)
    native_policy = load('native-sources.json')
    names = {(p['name'], p['version']) for p in bundle['crates']}
    expected_native = {(p['crate'], p['version']) for p in native_policy['packages']
                       if (p['crate'], p['version']) in names}
    if {(p['crate'], p['version']) for p in bundle['nativeSources']} != expected_native:
        raise ValueError('Native source inventory is incomplete')
    for native in bundle['nativeSources']:
        entries = [p for p in native_policy['packages'] if p['crate'] == native['crate'] and p['version'] == native['version']]
        if len(entries) != 1 or entries[0]['nativeFilesSha256'] != native['nativeFilesSha256']:
            raise ValueError('Unapproved native source in notice bundle')
        expected = {(x['path'], x.get('startLine'), x.get('endLine'), x['sha256'])
                    for c in entries[0]['components'] for x in c['notices']}
        actual = {(x['path'], x.get('startLine'), x.get('endLine'), sha256(x['text'].encode('utf-8')))
                  for x in native['notices']}
        if actual != expected:
            raise ValueError('Native original notice missing or modified')
    return bundle


def verify_output(directory):
    # Imported lazily to keep the generator usable inside the Linux compiler
    # image and avoid the build/provenance import cycle at module initialization.
    from provenance import read_member, member_names
    receipt = json.loads((directory / 'build-receipt.json').read_text(encoding='utf-8'))
    archive = directory / receipt['archive']
    if member_names(archive) != [receipt['binary'], 'LICENSE', 'NOTICE', *FILES, 'build-receipt.json']:
        raise ValueError('Archive is missing its third-party notice members')
    if json.loads(read_member(archive, 'build-receipt.json')) != receipt:
        raise ValueError('Archived third-party receipt differs')
    validate_bundle(read_member(archive, FILES[0]), read_member(archive, FILES[1]),
                    receipt['thirdPartyNotices'], receipt['rustTarget'])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--verify-output', type=Path)
    parser.add_argument('--target')
    parser.add_argument('--cargo')
    parser.add_argument('--tool-archive', type=Path)
    parser.add_argument('--output', type=Path)
    parser.add_argument('--build-log', type=Path)
    args = parser.parse_args()
    if args.verify_output:
        verify_output(args.verify_output)
        print('Archived third-party notices and policy checks passed')
        return
    if not all((args.target, args.cargo, args.tool_archive, args.output)):
        parser.error('Generation requires --target, --cargo, --tool-archive and --output')
    try:
        result = generate(args.target, args.cargo, args.tool_archive, args.output, args.build_log)
    except Exception as error:
        (args.output / 'third-party-result.json').write_bytes(canonical({'status': 'failed', 'error': str(error)}))
        raise
    print(json.dumps({'status': result['status'], 'crateCount': result['crateCount'], 'files': result['files']}))


if __name__ == '__main__':
    main()
