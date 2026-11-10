"""Read the release license policy and check resolved SPDX choices.

Only the license allow-list and exact-version exceptions are read from
deny.toml. This deliberately small reader supports the policy syntax used by
this repository, and refuses unsupported exception shapes instead of widening
them. No advisory, source or private-package policy is changed here.
"""
import ast
import hashlib
import json
from pathlib import Path
import re

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def canonical(value):
    return (json.dumps(value, sort_keys=True, ensure_ascii=True, indent=2) + '\n').encode()


def license_policy(path=None):
    text = (path or ROOT / 'deny.toml').read_text(encoding='utf-8')
    sections = re.split(r'(?m)^\[([^\n]+)\]\s*$', text)
    policy = {'allow': [], 'exceptions': []}
    for index in range(1, len(sections), 2):
        name, body = sections[index:index + 2]
        if name not in ('licenses', '[licenses.exceptions]'):
            continue
        match = re.search(r'(?m)^allow\s*=\s*(\[[^\]]*\])', body)
        if not match:
            raise ValueError('Missing license allow list in ' + name)
        allowed = ast.literal_eval(match.group(1))
        if not isinstance(allowed, list) or not all(isinstance(x, str) for x in allowed):
            raise ValueError('Invalid license allow list')
        if name == 'licenses':
            if not re.search(r'(?m)^private\s*=\s*\{\s*ignore\s*=\s*true\s*\}', body):
                raise ValueError('Unsupported deny.toml private license policy')
            policy['allow'] = allowed
        else:
            fields = dict(re.findall(r'(?m)^(name|version)\s*=\s*"([^"\n]+)"\s*$', body))
            if set(fields) != {'name', 'version'} or not re.fullmatch(r'=\d+\.\d+\.\d+(?:[-+][\w.-]+)?', fields['version']):
                raise ValueError('Release license exceptions require an exact name and =version')
            policy['exceptions'].append(dict(fields, allow=allowed))
    if not policy['allow']:
        raise ValueError('deny.toml has no license policy')
    return policy


def allowed_for(package, policy):
    allowed = set(policy['allow'])
    for exception in policy['exceptions']:
        if package['name'] == exception['name'] and package['version'] == exception['version'][1:]:
            allowed.update(exception['allow'])
    return allowed


def satisfies(expression, selected):
    """Evaluate AND/OR/WITH; identifiers remain exact SPDX strings.

    Cargo's historical slash-separated alternatives are treated as OR, as by
    cargo-about. Unknown syntax is an error, never an implicit permission.
    """
    expression = expression.replace('/', ' OR ')
    tokens = re.findall(r'\(|\)|[A-Za-z0-9.+-]+(?:\s+WITH\s+[A-Za-z0-9.+-]+)?', expression)
    if re.sub(r'\s+', '', ''.join(tokens)) != re.sub(r'\s+', '', expression):
        raise ValueError('Unsupported SPDX syntax: ' + expression)
    position = 0

    def atom():
        nonlocal position
        if position >= len(tokens):
            raise ValueError('Incomplete SPDX expression')
        token = tokens[position]
        position += 1
        if token == '(':
            value = either()
            if position >= len(tokens) or tokens[position] != ')':
                raise ValueError('Unclosed SPDX expression')
            position += 1
            return value
        if token in ('AND', 'OR', ')', 'WITH'):
            raise ValueError('Invalid SPDX atom: ' + token)
        return ' '.join(token.split()) in selected

    def both():
        nonlocal position
        value = atom()
        while position < len(tokens) and tokens[position] == 'AND':
            position += 1
            right = atom()
            value = value and right
        return value

    def either():
        nonlocal position
        value = both()
        while position < len(tokens) and tokens[position] == 'OR':
            position += 1
            right = both()
            value = value or right
        return value

    result = either()
    if position != len(tokens):
        raise ValueError('Unexpected SPDX tokens: ' + expression)
    return result


def check_crate(package, selected, policy):
    if not selected or not set(selected) <= allowed_for(package, policy):
        raise ValueError('License outside deny.toml for ' + package['name'] + ': ' + repr(selected))
    if not satisfies(package['license'], set(selected)):
        raise ValueError('Selected licenses do not satisfy metadata for ' + package['name'])
