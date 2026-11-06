#!/usr/bin/env python3
"""Generate the Desktop wire types from the small, serde-tagged Decision contract."""
import argparse
import re
from pathlib import Path

SOURCE = Path(__file__).resolve().parents[1] / 'crates/decision-api/src/lib.rs'
NAMES = ['ModelRef', 'Image', 'Question', 'Answer', 'Request', 'Input', 'Usage', 'Response']

def split_top(text):
    result, start, depth = [], 0, 0
    for i, ch in enumerate(text):
        if ch in '<{(': depth += 1
        elif ch in '>})': depth -= 1
        elif ch == ',' and depth == 0:
            if text[start:i].strip(): result.append(text[start:i].strip())
            start = i + 1
    if text[start:].strip(): result.append(text[start:].strip())
    return result

def ts_type(kind):
    kind = kind.strip()
    primitives = {'String': 'string', 'u64': 'number', 'f64': 'number', 'serde_json::Value': 'unknown'}
    if kind in primitives: return primitives[kind]
    for generic in ['Option', 'Vec', 'BTreeMap']:
        if kind.startswith(generic + '<') and kind.endswith('>'):
            inner = kind[len(generic)+1:-1]
            if generic == 'Option': return ts_type(inner) + ' | null'
            if generic == 'Vec': return 'Array<' + ts_type(inner) + '>'
            key, value = split_top(inner)
            if key != 'String': raise ValueError('unsupported map key: ' + key)
            return 'Record<string, ' + ts_type(value) + '>'
    if kind in NAMES: return 'Decision' + kind
    raise ValueError('unsupported Decision wire type: ' + kind)

def fields(body):
    result = []
    for field in split_top(body):
        optional = 'skip_serializing_if' in field
        field = re.sub(r'#\[[^\]]*\]', '', field).strip()
        field = re.sub(r'^pub\s+', '', field)
        name, kind = field.split(':', 1)
        result.append(name.strip() + ('?' if optional else '') + ': ' + ts_type(kind))
    return '; '.join(result)

def generate():
    source = re.sub(r'//[^\n]*', '', SOURCE.read_text())
    output = ['// Generated from harness-runtime/crates/decision-api/src/lib.rs. Do not edit.']
    for name in NAMES:
        match = re.search(r'pub (struct|enum) ' + name + r'\s*\{', source)
        if not match: raise ValueError('missing contract: ' + name)
        start = match.end(); depth = 1; end = start
        while depth:
            if source[end] == '{': depth += 1
            elif source[end] == '}': depth -= 1
            end += 1
        body = source[start:end-1]
        if match.group(1) == 'struct':
            output.append('export interface Decision' + name + ' { ' + fields(body) + ' }')
        else:
            variants = []
            for variant in split_top(body):
                variant = re.sub(r'#\[[^\]]*\]', '', variant).strip()
                item = re.fullmatch(r'(\w+)\s*\{(.*)\}', variant, re.S)
                if not item: raise ValueError('unsupported enum variant: ' + variant)
                tag = re.sub(r'(?<!^)(?=[A-Z])', '_', item.group(1)).lower()
                variants.append("{ type: '" + tag + "'; " + fields(item.group(2)) + ' }')
            output.append('export type Decision' + name + ' =\n  | ' + '\n  | '.join(variants) + ';')
    return '\n'.join(output) + '\n'

if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', type=Path)
    parser.add_argument('--check', type=Path)
    args = parser.parse_args()
    content = generate()
    if args.check:
        if args.check.read_text() != content: raise SystemExit('Decision generated types are stale')
    elif args.output: args.output.write_text(content)
    else: print(content, end='')
