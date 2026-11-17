"""Read this repository's workflow files for structure tests, with the standard library only.

`read` parses the YAML these workflows are written in: block mappings and
sequences, plain scalars (also continued over several lines), quoted scalars,
`|` and `>` block scalars, one-line flow collections and comments. Every
scalar is returned as a string. Anything else (anchors, aliases, tags, tabs,
duplicate keys, a quoted scalar or flow collection over several lines) raises
WorkflowSyntax, so a test fails instead of reading a workflow differently
from GitHub.

`condition_terms` splits a job or step condition into the terms that must all
be true, and refuses a condition that is not such a conjunction.
"""
import re

KEY = re.compile(r'([A-Za-z0-9_][A-Za-z0-9_.-]*):(?:[ ]+(.*))?$')
BLOCK_HEADER = re.compile(r'([|>])([+-]?)(?:[ ]+#.*)?$')
STATUS_FUNCTION = re.compile(r'\b(always|cancelled|failure|success)\s*\(', re.IGNORECASE)


class WorkflowSyntax(ValueError):
    """The text is outside what this reader models, or a condition is not a conjunction."""


def indentation(line):
    stripped = line.lstrip(' ')
    if stripped.startswith('\t'):
        raise WorkflowSyntax('tab in indentation: ' + line)
    return len(line) - len(stripped)


def ignorable(line):
    return not line.strip() or line.strip().startswith('#')


def next_content(lines, index):
    while index < len(lines) and ignorable(lines[index]):
        index += 1
    return index


def without_comment(text):
    """Drop a trailing comment. In a plain scalar `#` after whitespace starts one, even inside `'...'`."""
    match = re.search(r'(^|[ ])#', text)
    return (text[:match.start()] if match else text).rstrip()


def quoted(text):
    """Return the value of a one-line quoted scalar followed by nothing but a comment."""
    if text.startswith("'"):
        match = re.match(r"'((?:[^']|'')*)'(?:[ ]+#.*)?$", text)
        if match:
            return match.group(1).replace("''", "'")
    else:
        match = re.match(r'"([^"\\]*)"(?:[ ]+#.*)?$', text)
        if match:
            return match.group(1)
    raise WorkflowSyntax('unsupported quoted scalar: ' + text)


def plain(text, first=True):
    """Check one line of a plain scalar; only its first line is limited in how it may start."""
    starts_badly = first and (text[:1] in '&*!%@`|>?' or text.startswith(('- ', '<<')))
    if not text or starts_badly or ': ' in text or text.endswith(':'):
        raise WorkflowSyntax('unsupported plain scalar: ' + text)
    return text


def flow(text):
    """Parse a one-line `[a, b]` or `{ a: b }` of plain or quoted scalars."""
    body = without_comment(text) if not re.search(r'["\']', text) else text.rstrip()
    closing = {'[': ']', '{': '}'}[body[0]]
    if not body.endswith(closing) or re.search(r'[\[\]{}]', body[1:-1]):
        raise WorkflowSyntax('unsupported flow collection: ' + text)
    items = [item.strip() for item in body[1:-1].split(',')] if body[1:-1].strip() else []
    if any(not item for item in items):
        raise WorkflowSyntax('unsupported flow collection: ' + text)

    def scalar(item):
        return quoted(item) if item[0] in '\'"' else plain(item)

    if body[0] == '[':
        return [scalar(item) for item in items]
    result = {}
    for item in items:
        match = KEY.match(item)
        if not match or not match.group(2) or match.group(1) in result:
            raise WorkflowSyntax('unsupported flow mapping: ' + text)
        result[match.group(1)] = scalar(match.group(2).strip())
    return result


def block_scalar(header, lines, index, parent):
    """Read a `|` or `>` scalar whose lines are indented more than `parent`."""
    style, chomp = BLOCK_HEADER.match(header).groups()
    content = []
    while index < len(lines) and (not lines[index].strip() or indentation(lines[index]) > parent):
        content.append(lines[index])
        index += 1
    while content and not content[-1].strip():
        content.pop()
    if not content or not content[0].strip():
        raise WorkflowSyntax('unsupported block scalar')
    width = indentation(content[0])
    if any(line.strip() and indentation(line) < width for line in content):
        raise WorkflowSyntax('block scalar indentation')
    body = [line[width:] if line.strip() else '' for line in content]
    if style == '>':
        if any(not line or line.startswith(' ') for line in body):
            raise WorkflowSyntax('unsupported folded scalar')
        text = ' '.join(body)
    else:
        text = '\n'.join(body)
    if chomp == '+':
        raise WorkflowSyntax('unsupported block scalar chomping')
    return text + ('' if chomp == '-' else '\n'), index


def value(text, lines, index, parent):
    """Read the node that starts with `text` after a key or `- `; `parent` is that key's indentation."""
    if BLOCK_HEADER.match(text):
        return block_scalar(text, lines, index, parent)
    if text[0] in '\'"':
        result = quoted(text)
    elif text[0] in '[{':
        result = flow(text)
    else:
        # A plain scalar continues on the following more indented lines, folded with spaces.
        parts = [text]
        while index < len(lines) and lines[index].strip() and indentation(lines[index]) > parent \
                and not lines[index].strip().startswith('#'):
            if without_comment(parts[-1]) != parts[-1].rstrip():
                raise WorkflowSyntax('text continues after a comment: ' + lines[index])
            parts.append(lines[index].strip())
            index += 1
        result = ' '.join(plain(without_comment(part), not position) for position, part in enumerate(parts))
    following = next_content(lines, index)
    if following < len(lines) and indentation(lines[following]) > parent:
        raise WorkflowSyntax('unexpected indentation: ' + lines[following])
    return result, index


def node(lines, index, indent):
    if lines[index][indent:].startswith('- ') or lines[index][indent:] == '-':
        return sequence(lines, index, indent)
    return mapping(lines, index, indent)


def mapping(lines, index, indent):
    result = {}
    while True:
        index = next_content(lines, index)
        if index == len(lines) or indentation(lines[index]) < indent:
            return result, index
        match = KEY.match(lines[index][indent:]) if indentation(lines[index]) == indent else None
        if not match:
            raise WorkflowSyntax('unsupported line: ' + lines[index])
        key, rest = match.group(1), (match.group(2) or '').strip()
        if key in result:
            raise WorkflowSyntax('duplicate key: ' + key)
        index += 1
        if rest and not rest.startswith('#'):
            result[key], index = value(rest, lines, index, indent)
            continue
        following = next_content(lines, index)
        if following < len(lines) and indentation(lines[following]) > indent:
            result[key], index = node(lines, following, indentation(lines[following]))
        else:
            result[key] = ''


def sequence(lines, index, indent):
    result = []
    while True:
        index = next_content(lines, index)
        if index == len(lines) or indentation(lines[index]) < indent:
            return result, index
        text = lines[index][indent:]
        if indentation(lines[index]) != indent or not text.startswith('- ') or text[2:3] in ('', ' '):
            raise WorkflowSyntax('unsupported sequence line: ' + lines[index])
        if KEY.match(text[2:]):
            # `- key: value` starts a mapping whose keys sit two columns further in.
            lines[index] = ' ' * (indent + 2) + text[2:]
            item, index = mapping(lines, index, indent + 2)
        else:
            item, index = value(text[2:].strip(), lines, index + 1, indent)
        result.append(item)


def read(text):
    """Return the document, a mapping, with every scalar as a string."""
    lines = text.split('\n')
    if any(line.startswith(('---', '...', '%')) for line in lines):
        raise WorkflowSyntax('unsupported document marker or directive')
    document, index = mapping(lines, 0, 0)
    if next_content(lines, index) != len(lines):
        raise WorkflowSyntax('unsupported line: ' + lines[index])
    return document


def condition_terms(condition):
    """Return the terms of a condition that is one conjunction: `a && b && c`.

    Raises WorkflowSyntax for anything that could let the job or step run while a
    term is false or a needed job did not succeed: an `||` anywhere, a status
    function (it replaces the default requirement that every needed job
    succeeded), or `${{ }}` around only part of the text (GitHub then evaluates
    the whole as a non-empty string, which is true).
    """
    text = condition.strip()
    if text.startswith('${{') and text.endswith('}}'):
        text = text[3:-2].strip()
    if '${{' in text or '}}' in text:
        raise WorkflowSyntax('expression markers inside a condition: ' + condition)
    terms, start, depth, index, code = [], 0, 0, 0, []
    while index < len(text):
        if text[index] == "'":
            # A string literal; '' inside it is an escaped quote.
            closing = index + 1
            while True:
                closing = text.find("'", closing)
                if closing < 0:
                    raise WorkflowSyntax('unterminated string in a condition: ' + condition)
                if text[closing:closing + 2] != "''":
                    break
                closing += 2
            index = closing + 1
            continue
        if text.startswith('||', index):
            raise WorkflowSyntax('|| in a condition: ' + condition)
        if text.startswith('&&', index) and depth == 0:
            terms.append(text[start:index].strip())
            start = index = index + 2
            continue
        depth += {'(': 1, ')': -1}.get(text[index], 0)
        if depth < 0:
            raise WorkflowSyntax('unbalanced parentheses in a condition: ' + condition)
        code.append(text[index])
        index += 1
    terms.append(text[start:].strip())
    if depth or not all(terms) or STATUS_FUNCTION.search(''.join(code)):
        raise WorkflowSyntax('not a plain conjunction: ' + condition)
    return terms
