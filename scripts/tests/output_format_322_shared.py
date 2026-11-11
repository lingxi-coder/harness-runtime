#!/usr/bin/env python3
"""Compare explicitly selected raw subtrees and frame order in pinned captures.

This is focused contract evidence, never a whole-stream parity verdict.
"""
import argparse
import hashlib
import math
import json
import re
from pathlib import Path
from uuid import UUID

DECODER = json.JSONDecoder()
RESULT_CLOCKS = {
    'duration_ms', 'duration_api_ms', 'ttft_ms', 'ttft_stream_ms',
    'time_to_request_ms', 'first_content_frame_ms',
}


def field(source, target):
    """Return the original JSON value bytes, retaining spelling and key order."""
    at = source.index('{') + 1
    while True:
        while source[at].isspace():
            at += 1
        if source[at] == '}':
            raise KeyError(target)
        key, at = DECODER.raw_decode(source, at)
        while source[at].isspace():
            at += 1
        assert source[at] == ':'
        at += 1
        while source[at].isspace():
            at += 1
        start = at
        _, at = DECODER.raw_decode(source, at)
        if key == target:
            return source[start:at].encode('utf-8')
        while source[at].isspace():
            at += 1
        if source[at] == ',':
            at += 1
        else:
            assert source[at] == '}'
            raise KeyError(target)


def captures(root, fixture, label):
    files = sorted((root / fixture / label / 'processes').glob('*/stdout.bin'))
    assert files, (root, fixture, label)
    processes = []
    for p in files:
        raw = p.read_bytes().decode('utf-8').strip()
        if raw.startswith('['):
            rows = []
            at = 1
            while True:
                while raw[at].isspace(): at += 1
                if raw[at] == ']': break
                start = at
                value, at = DECODER.raw_decode(raw, at)
                rows.append((raw[start:at], value))
                while raw[at].isspace(): at += 1
                if raw[at] == ',': at += 1
                else: assert raw[at] == ']'
            processes.append(rows)
        else:
            processes.append([(line, json.loads(line)) for line in raw.splitlines() if line])
    return processes


def ledger(root, fixture, sdk_interval=True):
    labels = {}
    for label in ('native-a', 'native-b', 'harness'):
        selected = []
        for process in captures(root, fixture, label):
            for raw, row in process:
                if row.get('type') != 'result':
                    continue
                keys = list(row)
                if sdk_interval:
                    assert keys[keys.index('result_index') + 1] == 'model_usage_since_last_result', (label, keys)
                else:
                    assert 'model_usage_since_last_result' not in keys, (label, keys)
                fields = ('total_cost_usd', 'usage', 'modelUsage') + (('model_usage_since_last_result',) if sdk_interval else ())
                selected.append({key: field(raw, key) for key in fields})
        assert selected, (label, fixture)
        labels[label] = selected
    reference = labels['native-a']
    assert labels['native-b'] == reference, f'{fixture}: native ledger is unstable'
    for index, (expected, actual) in enumerate(zip(reference, labels['harness'])):
        for key in expected:
            assert actual[key] == expected[key], f'{fixture}, result {index}, {key}: {actual[key]!r} != {expected[key]!r}'
    assert len(labels['harness']) == len(reference), f'{fixture}: result counts differ'
    return {'fixture': fixture, 'result_count': len(reference), 'raw_fields': [
        {key: {'bytes': len(value), 'sha256': hashlib.sha256(value).hexdigest(), 'value': json.loads(value)}
         for key, value in row.items()} for row in reference]}


def signatures(process):
    result = []
    for _, row in process:
        if row.get('type') == 'stream_event':
            event = row['event']
            suffix = str(event['index']) if 'index' in event else ''
            result.append(event['type'] + (':' + suffix if suffix else ''))
        elif row.get('type') == 'assistant':
            content = row['message']['content']
            result.append('assistant:' + ','.join(block['type'] + (':' + block['id'] if block['type'] == 'tool_use' else '') for block in content))
        elif row.get('type') == 'user' and any(block.get('type') == 'tool_result' for block in row.get('message', {}).get('content', []) if isinstance(block, dict)):
            result.append('user:tool_result:' + ','.join(block['tool_use_id'] for block in row['message']['content'] if block.get('type') == 'tool_result'))
    return result


def ordering(root, fixture):
    labels = {}
    messages = {}
    for label in ('native-a', 'native-b', 'harness'):
        processes = captures(root, fixture, label)
        labels[label] = [signatures(process) for process in processes]
        messages[label] = [[field(raw, 'message') for raw, row in process if row.get('type') == 'assistant'] for process in processes]
        if fixture in ('max-turns', 'multi-tools-ready'):
            tool_results = [block for process in processes for _, row in process if row.get('type') == 'user'
                            for block in row.get('message', {}).get('content', []) if isinstance(block, dict) and block.get('type') == 'tool_result']
            assert len(tool_results) == (2 if fixture == 'multi-tools-ready' else 1), (fixture, label, 'missing tool result')
            assert all(block.get('is_error') is False and block.get('content') == 'HEADLESS_TOOL' for block in tool_results), (fixture, label, 'tool did not execute fixture command')
    assert messages['native-b'] == messages['native-a'], f'{fixture}: native assistant messages unstable'
    assert messages['harness'] == messages['native-a'], f'{fixture}: assistant message bytes differ'
    assert labels['native-b'] == labels['native-a'], f'{fixture}: native frame order is unstable: {labels!r}'
    assert labels['harness'] == labels['native-a'], f'{fixture}: frame order differs: {labels!r}'
    return {'fixture': fixture, 'order': labels['native-a']}


def shared_rows(root, fixture, kinds, volatile):
    """Compare every reference field except explicitly named generated values.

    Extra LingXi fields are retained in the captures and excluded from this
    shared-contract projection. Values are sliced from the original wire JSON;
    this never parses and reserializes a value to make it equal.
    """
    labels = {label: [[(raw, row) for raw, row in process if row.get('type') in kinds]
                      for process in captures(root, fixture, label)]
              for label in ('native-a', 'native-b', 'harness')}
    reference = labels['native-a']
    assert any(reference), (fixture, 'no selected rows')
    checked = []
    for label in ('native-b', 'harness'):
        assert len(labels[label]) == len(reference), (fixture, label, 'process count')
        for process_index, (expected, actual) in enumerate(zip(reference, labels[label])):
            assert len(expected) == len(actual), (fixture, label, 'frame count', process_index)
            for row_index, ((expected_raw, expected_row), (actual_raw, actual_row)) in enumerate(zip(expected, actual)):
                context = (fixture, label, process_index, row_index, expected_row['type'])
                assert all(key in actual_row for key in expected_row), (context, 'missing shared field')
                assert [key for key in actual_row if key in expected_row] == list(expected_row), (context, 'shared key order')
                for key in expected_row:
                    if key == 'uuid':
                        assert str(UUID(actual_row[key])) == actual_row[key], (context, 'invalid uuid')
                    elif key in RESULT_CLOCKS:
                        assert type(actual_row[key]) in (int, float) and math.isfinite(actual_row[key]) and actual_row[key] >= 0, (context, key)
                    elif key == 'timestamp':
                        assert re.fullmatch(r'\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z', actual_row[key]), (context, 'timestamp format')
                    elif key not in volatile:
                        assert field(actual_raw, key) == field(expected_raw, key), (context, key, field(actual_raw, key), field(expected_raw, key))
                if label == 'harness':
                    checked.append({'process': process_index, 'row': row_index, 'type': expected_row['type'],
                                    'shared_keys': list(expected_row),
                                    'raw_equal_fields': [key for key in expected_row if key not in volatile]})
    return {'fixture': fixture, 'frames': checked, 'generated_fields': sorted(volatile)}


def frame_identity(root, fixture):
    checked = {}
    for label in ('native-a', 'native-b', 'harness'):
        processes = captures(root, fixture, label)
        count = 0
        for process_index, process in enumerate(processes):
            session_root = root / fixture / label / 'processes' / str(process_index) / 'sessions' / 'projects'
            stored = {}
            for path in session_root.rglob('*.jsonl'):
                for line in path.read_text().splitlines():
                    row = json.loads(line)
                    if row.get('type') in ('assistant', 'user') and row.get('uuid'):
                        stored[row['uuid']] = row
            for _, row in process:
                if row.get('type') not in ('assistant', 'user'):
                    continue
                assert row['uuid'] in stored, (fixture, label, 'public frame missing from transcript', row['uuid'])
                accepted = stored[row['uuid']]
                for public_key, stored_key in [('type', 'type'), ('timestamp', 'timestamp'), ('session_id', 'sessionId')]:
                    assert row[public_key] == accepted[stored_key], (fixture, label, row['uuid'], public_key)
                assert row['message']['content'] == accepted['message']['content'], (fixture, label, row['uuid'], 'content')
                count += 1
        checked[label] = count
    return {'fixture': fixture, 'accepted_frames': checked}


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('--ledger', nargs=2, action='append', default=[], metavar=('ROOT', 'FIXTURE'))
    parser.add_argument('--json-usage', nargs=2, action='append', default=[], metavar=('ROOT', 'FIXTURE'))
    parser.add_argument('--ordering', nargs=2, action='append', default=[], metavar=('ROOT', 'FIXTURE'))
    parser.add_argument('--shared-results', nargs=2, action='append', default=[], metavar=('ROOT', 'FIXTURE'))
    parser.add_argument('--shared-frames', nargs=2, action='append', default=[], metavar=('ROOT', 'FIXTURE'))
    args = parser.parse_args()
    assert args.ledger or args.ordering or args.json_usage or args.shared_results or args.shared_frames
    print(json.dumps({'scope': 'raw shared-field bytes and shared key order; LingXi extensions retained; generated IDs/clocks checked separately',
                      'ledgers': [ledger(Path(root), fixture) for root, fixture in args.ledger],
                      'json_usage': [ledger(Path(root), fixture, sdk_interval=False) for root, fixture in args.json_usage],
                      'ordering': [ordering(Path(root), fixture) for root, fixture in args.ordering],
                      'shared_results': [shared_rows(Path(root), fixture, {'result'}, RESULT_CLOCKS | {'uuid'}) for root, fixture in args.shared_results],
                      'shared_frames': [shared_rows(Path(root), fixture, {'assistant', 'user', 'stream_event'}, RESULT_CLOCKS | {'uuid', 'timestamp'}) for root, fixture in args.shared_frames],
                      'frame_identity': [frame_identity(Path(root), fixture) for root, fixture in args.shared_frames]}, indent=2))


if __name__ == '__main__':
    main()
