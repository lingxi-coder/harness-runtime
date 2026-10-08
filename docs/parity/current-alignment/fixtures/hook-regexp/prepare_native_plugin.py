#!/usr/bin/env python3
"""Generate a temporary Claude Code plugin from the shared matcher corpus."""
import json
import shlex
from pathlib import Path
import sys

root = Path(__file__).resolve().parent
output = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else root / 'run-output'
output.mkdir(parents=True, exist_ok=True)
plugin = output / 'native-plugin'
result_file = output / 'fired.log'
script = plugin / 'scripts' / 'record-match.sh'
inputs = output / 'inputs'
for directory in (plugin / '.claude-plugin', plugin / 'hooks', plugin / 'scripts', inputs):
    directory.mkdir(parents=True, exist_ok=True)
(plugin / '.claude-plugin' / 'plugin.json').write_text(
    json.dumps({'name': 'native-hook-regexp-fixture'}, separators=(',', ':')) + '\n'
)
script.write_text('#!/bin/sh\ncat > "$3"\nprintf \'%s\\n\' "$1" >> "$2"\n')
for prior in inputs.glob('*.json'):
    prior.unlink()
cases = json.loads((root / 'matcher-cases.json').read_text())['native_comparable']

hooks = []
for case in cases:
    matcher = json.loads(case['pattern_json'])
    query = json.loads(case['query_json'])
    if query != 'startup':
        raise SystemExit(f"Native SessionStart fixture expects query 'startup': {case['id']}")
    command = ' '.join((
        '/bin/sh', shlex.quote(str(script)),
        shlex.quote(case['id']), shlex.quote(str(result_file)),
        shlex.quote(str(inputs / f"{case['id']}.json")),
    ))
    hooks.append({
        'matcher': matcher,
        'hooks': [{'type': 'command', 'command': command}],
    })

(plugin / 'hooks' / 'hooks.json').write_text(
    json.dumps({'hooks': {'SessionStart': hooks}}, ensure_ascii=True, indent=2) + '\n'
)
print(f'Wrote {plugin / "hooks" / "hooks.json"}')
print(f'Native hook callbacks append IDs to {result_file}')
