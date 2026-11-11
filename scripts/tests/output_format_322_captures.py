#!/usr/bin/env python3
"""Verify the explicitly scoped shared contract without rewriting wire values."""
import argparse
import hashlib
import json
from pathlib import Path

import output_format_322_shared as check

PIN = 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450'
CASES = {
    'ledger': ['ledger-json-loop', 'ledger-verbose-json-loop', 'ledger-tool-loop',
               'sdk-multi-turn', 'sdk-resume', 'sdk-interrupt'],
    'immediate': ['max-turns'], 'slow': ['max-turns'], 'multi': ['multi-tools-ready'],
    'text': ['partial-stream', 'print-json-verbose'],
    'resume-loop': ['ledger-resume-loop'],
    'stream-errors': ['schema-retry-stream', 'budget-stream'],
    'errors': ['schema-success', 'schema-retry-exhausted', 'budget-limit',
               'permission-deny-none', 'permission-host-allow', 'permission-host-deny'],
}


def verify(root):
    result = {'native_version': '2.1.322', 'native_sha256': PIN,
              'scope': 'Raw shared result fields, shared assistant/tool/partial frames, readiness order, and accepted transcript identity; LingXi extensions retained.',
              'generated_values': 'UUIDs and elapsed clocks are validated rather than equalized; public assistant/tool UUID and timestamp must match their own accepted transcript row.',
              'shared_results': [], 'ledgers': [], 'ordering': [], 'shared_frames': [],
              'frame_identity': [], 'permission_requests': [], 'diagnostic_data_differences': []}
    binaries = set()
    for group, fixtures in CASES.items():
        report = json.loads((root / group / 'report.json').read_text())
        assert report['baseline']['version'] == '2.1.322'
        assert report['baseline']['binary']['sha256'] == PIN
        assert report['credentialMode'] == 'fixed dummy only'
        assert report['nativeNativeCompletedBeforeHarness'] is True
        binaries.add(report['harnessBinary']['sha256'])
        for fixture in fixtures:
            process_counts = []
            exits = {}
            for label in ('native-a', 'native-b', 'harness'):
                observations = report['captures'][f'{fixture}/{label}']['observations']
                assert all(not observation['timedOut'] for observation in observations)
                assert all(observation['exit']['signal'] is None for observation in observations)
                exits[label] = [observation['exit']['code'] for observation in observations]
                process_counts.append(len(observations))
                for path in (root / group / fixture / label / 'processes').glob('*/stdout.bin'):
                    raw = path.read_bytes()
                    assert raw.endswith(b'\n') and b'\r\n' not in raw, path
            assert len(set(process_counts)) == 1, (group, fixture, 'process count')
            assert exits['native-a'] == exits['native-b'] == exits['harness'], (group, fixture, exits)
            result['shared_results'].append(check.shared_rows(root / group, fixture, {'result'}, check.RESULT_CLOCKS | {'uuid'}))
    assert len(binaries) == 1, 'candidate changed during captures'
    result['harness_sha256'] = binaries.pop()
    for fixture in ('sdk-multi-turn', 'sdk-resume', 'sdk-interrupt', 'ledger-tool-loop'):
        result['ledgers'].append(check.ledger(root / 'ledger', fixture))
    for fixture in ('ledger-json-loop', 'ledger-verbose-json-loop'):
        result['ledgers'].append(check.ledger(root / 'ledger', fixture, sdk_interval=False))
    result['ledgers'].append(check.ledger(root / 'resume-loop', 'ledger-resume-loop'))
    for fixture in ('schema-retry-stream', 'budget-stream'):
        result['ledgers'].append(check.ledger(root / 'stream-errors', fixture))
    for group, fixture in [('immediate', 'max-turns'), ('slow', 'max-turns'),
                           ('multi', 'multi-tools-ready'), ('text', 'partial-stream'),
                           ('text', 'print-json-verbose')]:
        result['ordering'].append(check.ordering(root / group, fixture))
    for group, fixture in [('immediate', 'max-turns'), ('slow', 'max-turns'),
                           ('multi', 'multi-tools-ready'), ('text', 'partial-stream'),
                           ('text', 'print-json-verbose'), ('errors', 'permission-host-allow'),
                           ('errors', 'permission-host-deny'), ('errors', 'permission-deny-none')]:
        result['shared_frames'].append(check.shared_rows(root / group, fixture,
            {'assistant', 'user', 'stream_event'}, check.RESULT_CLOCKS | {'uuid', 'timestamp'}))
        result['frame_identity'].append(check.frame_identity(root / group, fixture))
    for fixture in ('permission-host-allow', 'permission-host-deny'):
        rows = {}
        for label in ('native-a', 'native-b', 'harness'):
            raw, frame = next((raw, frame) for process in check.captures(root / 'errors', fixture, label)
                              for raw, frame in process if frame.get('type') == 'control_request')
            request = check.field(raw, 'request').decode()
            mandatory = ['subtype', 'tool_name', 'display_name', 'input', 'description', 'tool_use_id']
            assert [key for key in frame['request'] if key in mandatory] == mandatory
            rows[label] = {key: check.field(request, key) for key in mandatory}
        assert rows['native-a'] == rows['native-b'] == rows['harness'], fixture
        result['permission_requests'].append({'fixture': fixture, 'raw_equal_fields': mandatory,
            'optional_context': 'Native path suggestions and blocked_path are supplied by its permission policy; LingXi retains its own policy facts.'})
    result['fixture_count'] = sum(map(len, CASES.values()))
    result['result_count'] = sum(len(row['frames']) for row in result['shared_results'])
    result['verified_contract_passed'] = True
    result['shared_payload_contract_byte_equal'] = True
    result['all_output_values_byte_equal'] = False
    result['limitations'] = [
        'Product/configuration data in init (skills, agents, plugins, capabilities, credential source) is truthful to each product and is not made identical.',
        'These controlled captures use dummy credentials; live provider/account acceptance is recorded in its separate receipt. The cost suite also covers session isolation and signed corrections.',
        'Unit-suite and live-account acceptance are recorded separately in the source/test manifest rather than inferred from these controlled captures.',
        'This is finite fixture evidence against the isolated source snapshot, not a claim about every possible provider/tool/control-plane trace.',
    ]
    return result


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('captures_root', type=Path)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    text = json.dumps(verify(args.captures_root), indent=2) + '\n'
    if args.output:
        args.output.write_text(text)
    else:
        print(text, end='')


if __name__ == '__main__':
    main()
