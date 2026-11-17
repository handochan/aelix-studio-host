"""Fail-closed inventory of native source shipped inside resolved packages.

The inventory is deliberately conservative: all C/C++/assembly/header and
prebuilt-library files in a selected package are checked, including disabled
backends and tests. Cargo build-script messages separately describe the actual
link operation; a system-library path never exempts packaged source from audit.
"""
from pathlib import Path
import re

from license_policy import canonical, sha256

NATIVE_EXTENSIONS = {'.c', '.h', '.cc', '.cpp', '.cxx', '.hpp', '.hxx', '.s', '.asm',
                     '.m', '.mm', '.inc', '.inl', '.hh', '.ipp', '.tcc', '.sx', '.nasm',
                     '.c++', '.h++', '.cppm', '.ixx', '.def', '.pl', '.i', '.ii', '.cu', '.cuh',
                     '.a', '.lib', '.o', '.obj', '.so', '.dylib', '.dll'}
NOTICE_NAME = re.compile(r'(?i)^(licen[cs]es?|copying|copyright|notice|authors)(?:$|[._-])')
NATIVE_BINARY_EXTENSIONS = {'.a', '.lib', '.o', '.obj', '.so', '.dylib', '.dll'}


def source_files(root):
    return sorted((p for p in root.rglob('*') if p.is_file() and '.git' not in p.relative_to(root).parts
                   and p.name not in ('.cargo-ok', '.cargo-checksum.json')),
                  key=lambda p: p.relative_to(root).as_posix())


def package_hash(root):
    return sha256(canonical([[p.relative_to(root).as_posix(), sha256(p.read_bytes())] for p in source_files(root)]))


def is_notice(path):
    return bool(NOTICE_NAME.match(path.name)) or any(p.lower() in ('licenses', 'licences') for p in path.parts[:-1])


def native_inventory(root):
    return [[p.relative_to(root).as_posix(), sha256(p.read_bytes())]
            for p in source_files(root) if is_native_file(p)]


def is_native_file(path):
    return path.suffix.lower() in NATIVE_EXTENSIONS or (
        path.suffix.lower() == '.in' and Path(path.stem).suffix.lower() in NATIVE_EXTENSIONS)


def is_native_text_file(path):
    return is_native_file(path) and path.suffix.lower() not in NATIVE_BINARY_EXTENSIONS


def read_notice(root, entry):
    data = (root / entry['path']).read_bytes()
    if 'startLine' in entry:
        data = b''.join(data.splitlines(keepends=True)[entry['startLine'] - 1:entry['endLine']])
    if not data or sha256(data) != entry['sha256']:
        raise ValueError('Missing or changed original native notice: ' + entry['path'])
    return data


def check_native(package, root, policy, linked=None):
    inventory = native_inventory(root)
    named = [p for p in policy['packages'] if p['crate'] == package['name']]
    if not inventory and not package.get('links') and not named and not any(
            p.get('linked_libs') or p.get('linked_paths') for p in (linked or [])):
        return None
    if not named:
        raise ValueError('Unlisted native source/links package: ' + package['name'])
    matches = [p for p in named if p['version'] == package['version']]
    if len(matches) != 1:
        raise ValueError('Native source version mismatch: ' + package['name'])
    entry, = matches
    if package_hash(root) != entry['packagedFilesSha256']:
        raise ValueError('Native package source hash mismatch: ' + package['name'])
    archive = root.parents[2] / 'cache' / root.parent.name / (package['name'] + '-' + package['version'] + '.crate')
    if not archive.is_file() or sha256(archive.read_bytes()) != entry['sourceArchiveSha256']:
        raise ValueError('Native source archive hash mismatch: ' + package['name'])
    if entry['nativeFilesSha256'] != sha256(canonical(inventory)):
        raise ValueError('Native source hash mismatch (new, missing or changed source): ' + package['name'])
    if entry['links'] != package.get('links'):
        raise ValueError('Native links declaration changed: ' + package['name'])
    build_scripts = [[p.relative_to(root).as_posix(), sha256(p.read_bytes())]
                     for p in source_files(root) if p.suffix == '.rs'
                     and (p.name == 'build.rs' or p.relative_to(root).parts[0] in ('builder', 'build'))]
    if sha256(canonical(build_scripts)) != entry['buildScriptsSha256']:
        raise ValueError('Native build recipe changed: ' + package['name'])
    covered = set()
    components = []
    originals = {}
    for component in entry['components']:
        files = [item for item in inventory if any(item[0].startswith(p) for p in component['sourcePrefixes'])]
        if sha256(canonical(files)) != component['sourceSha256']:
            raise ValueError('Native component hash mismatch: ' + component['component'])
        covered.update(p[0] for p in files)
        if not component['licenseConditions'] or not component['notices']:
            raise ValueError('Native component has no original license notice: ' + component['component'])
        for notice in component['notices']:
            key = (notice['path'], notice.get('startLine'), notice.get('endLine'))
            originals[key] = (notice, read_notice(root, notice))
        components.append({key: component[key] for key in
                           ('component', 'version', 'licenseConditions', 'sourceSha256', 'license', 'approval', 'distribution')
                           if key in component})
    if covered != {p[0] for p in inventory}:
        raise ValueError('Unlisted native source files in ' + package['name'])
    linked = linked or []
    libs = sorted({lib for event in linked for lib in event.get('linked_libs', [])})
    return {
        'crate': package['name'], 'version': package['version'], 'links': package.get('links'),
        'nativeFilesSha256': entry['nativeFilesSha256'], 'nativeFileCount': len(inventory),
        'sourceArchiveSha256': entry['sourceArchiveSha256'], 'components': components,
        'linkedLibraries': libs,
        'linkage': ('static' if any(re.match(r'^static(?:=|:)', x) for x in libs) else
                    'system-or-dynamic' if libs else 'no-native-link-emitted'),
        'notices': [dict(spec, text=data.decode('utf-8')) for spec, data in originals.values()],
    }


def copyright_comments(text):
    """Retain original comment blocks, not reconstructed copyright statements."""
    for match in re.finditer(r'/\*.*?\*/|(?:^[ \t]*//[^\n]*(?:\n|$))+|(?:^[ \t]*\#[^\n]*(?:\n|$))+',
                             text, re.S | re.M):
        block = match.group()
        if re.search(r'(?i)copyright|SPDX-License-Identifier|author disclaims copyright', block):
            yield block
