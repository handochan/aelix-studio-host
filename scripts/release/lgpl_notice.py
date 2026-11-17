"""Owner-approved LGPL corresponding-source notice, shared by archives and notes."""
import hashlib
import json
from pathlib import Path
import re
import urllib.request

PUBLIC_SOURCE = 'https://github.com/handochan/aelix-studio-host'
DECISION = PUBLIC_SOURCE + '/issues/20#issuecomment-6771825765'
HERE = Path(__file__).resolve().parent


def source_asset():
    """Bind the unmodified corresponding-source asset to both reviewed policy and Cargo.lock."""
    policy = json.loads((HERE / 'native-sources.json').read_text(encoding='utf-8'))
    packages = [p for p in policy['packages'] if p['crate'] == 'libgit2-sys']
    if len(packages) != 1:
        raise ValueError('LGPL source asset requires one reviewed libgit2-sys package')
    package = packages[0]
    locked = []
    for block in (HERE.parents[1] / 'Cargo.lock').read_text(encoding='utf-8').split('[[package]]')[1:]:
        fields = dict(re.findall(r'(?m)^(name|version|source|checksum) = "([^"\n]+)"$', block))
        if fields.get('name') == package['crate']:
            locked.append(fields)
    if len(locked) != 1 or locked[0].get('version') != package['version'] or \
            locked[0].get('source') != 'registry+https://github.com/rust-lang/crates.io-index' or \
            locked[0].get('checksum') != package['sourceArchiveSha256']:
        raise ValueError('LGPL source asset policy differs from Cargo.lock')
    name = package['crate'] + '-' + package['version'] + '.crate'
    return {'crate': package['crate'], 'version': package['version'], 'name': name,
            'url': 'https://static.crates.io/crates/' + package['crate'] + '/' + name,
            'sha256': package['sourceArchiveSha256']}


def verify_source_asset(directory, record=None):
    expected = source_asset()
    if record is not None and record != expected:
        raise ValueError('LGPL source asset record differs from notice/Cargo.lock')
    path = directory / expected['name']
    if hashlib.sha256(path.read_bytes()).hexdigest() != expected['sha256']:
        raise ValueError('LGPL source asset checksum mismatch: ' + expected['name'])
    return expected


def download_source_asset(directory):
    spec = source_asset()
    with urllib.request.urlopen(spec['url'], timeout=120) as response:
        data = response.read()
    if hashlib.sha256(data).hexdigest() != spec['sha256']:
        raise ValueError('LGPL source download checksum mismatch')
    (directory / spec['name']).write_bytes(data)
    return verify_source_asset(directory, spec)


def source_context(label, commit):
    # Import lazily: provenance assembles builds that themselves import this helper.
    from provenance import TAG_PATTERN
    if not re.fullmatch(r'[0-9a-f]{40}', commit or ''):
        raise ValueError('LGPL source notice requires the exact source commit')
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]{0,127}', label or ''):
        raise ValueError('LGPL source notice requires a safe build label')
    tag = label if TAG_PATTERN.fullmatch(label) else None
    ref = tag or commit
    return {'repository': PUBLIC_SOURCE, 'label': label, 'releaseTag': tag, 'commit': commit,
            'sourceUrl': PUBLIC_SOURCE + '/tree/' + ref,
            'cargoLockUrl': PUBLIC_SOURCE + '/blob/' + ref + '/Cargo.lock',
            'buildUrl': PUBLIC_SOURCE + '/blob/' + ref + '/README.md#rebuilding-with-modified-lgpl-sources',
            'sourceAsset': source_asset()}


def required_components(native_sources, policy):
    active = {(p['crate'], p['version']) for p in native_sources}
    result = []
    for package in policy['packages']:
        if (package['crate'], package['version']) not in active:
            continue
        for component in package['components']:
            if component.get('license') != 'LGPL-2.1-or-later':
                continue
            if component.get('approval', {}).get('status') != 'approved':
                raise ValueError('LGPL component has no approval: ' + component['component'])
            result.append(component)
    return result


def render(context, components):
    if not components:
        return ''
    if not context or context != source_context(context.get('label'), context.get('commit')):
        raise ValueError('Missing or inconsistent LGPL corresponding-source context')
    statements = []
    for component in components:
        distribution = component.get('distribution')
        if distribution == 'static':
            statements.append(component['component'] + ' is licensed under LGPL-2.1-or-later and is statically linked into this host through bundled libgit2.')
        elif distribution == 'source-superset':
            statements.append(component['component'] + ' (.def source superset) is retained under LGPL-2.1-or-later; these definition files are not used by the MSVC build.')
        else:
            raise ValueError('LGPL component distribution is unspecified: ' + component['component'])
    tag = context['releaseTag'] or '(none; dry run/custom build ' + context['label'] + ')'
    asset = context['sourceAsset']
    return '\n'.join([
        'LGPL-2.1-or-later components and corresponding source',
        *statements,
        'Release tag: ' + tag,
        'Source commit: ' + context['commit'],
        'Unmodified corresponding-source release asset (also included in dry-run assets): ' + asset['name'],
        'Direct crates.io source URL: ' + asset['url'],
        'Source asset SHA-256 (Cargo.lock checksum): ' + asset['sha256'],
        'Corresponding source (release tag, or exact commit for a dry run): ' + context['sourceUrl'],
        'Dependency versions and source identities (Cargo.lock): ' + context['cargoLockUrl'],
        'To rebuild with modified xdiff, check out that tag/commit, obtain the dependency sources identified by Cargo.lock, apply your changes to libgit2-sys/libgit2/deps/xdiff in a local dependency fork, and follow the README build procedure: ' + context['buildUrl'],
        'The LGPL license copies and original component notices are reproduced in the notice bundle.',
    ])
