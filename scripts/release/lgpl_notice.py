"""Owner-approved LGPL corresponding-source notice, shared by archives and notes."""
import re

PUBLIC_SOURCE = 'https://github.com/handochan/aelix-studio-host'
DECISION = PUBLIC_SOURCE + '/issues/20#issuecomment-6771825765'
TAG_PATTERN = re.compile(r'^host-v(\d+\.\d+\.\d+)-aelix\.([1-9]\d*)$')


def source_context(label, commit):
    if not re.fullmatch(r'[0-9a-f]{40}', commit or ''):
        raise ValueError('LGPL source notice requires the exact source commit')
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]{0,127}', label or ''):
        raise ValueError('LGPL source notice requires a safe build label')
    tag = label if TAG_PATTERN.fullmatch(label) else None
    ref = tag or commit
    return {'repository': PUBLIC_SOURCE, 'label': label, 'releaseTag': tag, 'commit': commit,
            'sourceUrl': PUBLIC_SOURCE + '/tree/' + ref,
            'cargoLockUrl': PUBLIC_SOURCE + '/blob/' + ref + '/Cargo.lock',
            'buildUrl': PUBLIC_SOURCE + '/blob/' + ref + '/README.md#building-locally'}


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
    return '\n'.join([
        'LGPL-2.1-or-later components and corresponding source',
        *statements,
        'Release tag: ' + tag,
        'Source commit: ' + context['commit'],
        'Corresponding source (release tag, or exact commit for a dry run): ' + context['sourceUrl'],
        'Dependency versions and source identities (Cargo.lock): ' + context['cargoLockUrl'],
        'To rebuild with modified xdiff, check out that tag/commit, obtain the dependency sources identified by Cargo.lock, apply your changes to libgit2-sys/libgit2/deps/xdiff in a local dependency fork, and follow the README build procedure: ' + context['buildUrl'],
        'The LGPL license copies and original component notices are reproduced in the notice bundle.',
    ])
