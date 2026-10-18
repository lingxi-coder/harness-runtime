"""Assemble existing evidence without converting source/regression to byte passes."""
import hashlib
import argparse
import json
import re
import shutil
from pathlib import Path

HERE = Path(__file__).resolve().parent
BASE = HERE.parents[1]
DOC = BASE / 'harness-runtime/docs/parity/current-alignment'
DEST = DOC / 'evidence/final-ledger-r376'
SOURCE_PROOF_FILES = (
    'r6-test-only-followup-proof.json',
    'r6-test-only-followup.patch',
    'r8-r9-source-followup-proof.json',
    'r8-r9-production-delta.patch',
    'r8-r10-source-followup-proof.json',
    'r8-r10-production-delta.patch',
)
SOURCE_PROOF_TOOLS = (
    'finalize-repair-receipt.py',
    'make-r8-final-source-proof.py',
    'make-r8-r9-source-proof.py',
)
H_BASELINE = '4d6d0d6c428e6624a303f5e2a5c96eb4a392f1a1'


def read(name):
    return json.loads((HERE / name).read_text())


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_artifact_items():
    """Return the read-only copy plan used by the standalone closure check."""
    items = {name: HERE / name for name in SOURCE_PROOF_FILES + SOURCE_PROOF_TOOLS}
    for gate_dir in sorted(HERE.glob('harness-repair-gates*')):
        if not gate_dir.is_dir():
            continue
        for path in sorted(gate_dir.rglob('*')):
            if path.is_file():
                items[str(path.relative_to(HERE))] = path
    for relative, source in items.items():
        assert source.is_file() and not source.is_symlink(), (relative, source)
    return items


def resolve_source_ref(raw):
    path = Path(raw)
    assert not path.is_absolute(), raw
    candidate = HERE / path
    assert candidate.exists() and not candidate.is_symlink(), (raw, candidate)
    resolved = candidate.resolve()
    assert resolved.is_relative_to(HERE.resolve()), (raw, resolved)
    assert resolved.is_file() and not resolved.is_symlink(), (raw, resolved)
    return resolved


def check_source_artifact_closure():
    """Validate proof-to-file references without loading final receipts or assembling output."""
    copy_items = source_artifact_items()

    def copied_reference(raw):
        source = resolve_source_ref(raw)
        destination = source.relative_to(HERE.resolve()).as_posix()
        assert destination in copy_items, (raw, destination)
        assert copy_items[destination].resolve() == source, (raw, copy_items[destination], source)
        return source

    gate_dirs = {f'r{number}': HERE / f'harness-repair-gates-r{number}' for number in range(6, 11)}
    assert all(path.is_dir() for path in gate_dirs.values()), gate_dirs
    gate_snapshots = {}
    for name, directory in gate_dirs.items():
        base_head = (directory / 'base-head.txt').read_text(encoding='utf-8').strip()
        assert base_head == H_BASELINE, (name, base_head)
        patch = directory / 'source.patch'
        current = directory / 'source-current.patch'
        sidecar = directory / 'source.patch.sha256'
        for path in (patch, current, sidecar):
            assert path.is_file() and not path.is_symlink(), path
        patch_sha = sha(patch)
        if name != 'r6':
            assert current.read_bytes() == patch.read_bytes(), name
        assert sidecar.read_text(encoding='utf-8').split()[0] == patch_sha, name
        gate_snapshots[name] = (patch, current, patch_sha)

    r6 = json.loads((HERE / SOURCE_PROOF_FILES[0]).read_text(encoding='utf-8'))
    r6_delta = copied_reference(r6['test_only_delta_patch'])
    assert r6['schema'] == 'r376-test-only-followup-proof-v1' and r6['status'] == 'verified'
    assert r6['path'] == 'crates/runtime/src/headless/run/control.rs'
    assert sha(r6_delta) == r6['test_only_delta_patch_sha256']
    assert r6['previous_source_patch_sha256'] == gate_snapshots['r6'][2]
    assert gate_snapshots['r6'][1].read_bytes() == gate_snapshots['r7'][0].read_bytes()
    assert r6['current_source_patch_sha256'] == sha(gate_snapshots['r6'][1]) == gate_snapshots['r7'][2]

    r8_r9 = json.loads((HERE / SOURCE_PROOF_FILES[2]).read_text(encoding='utf-8'))
    r8_r9_delta = copied_reference(r8_r9['delta']['patch'])
    assert r8_r9['schema'] == 'r376-r8-r9-single-production-file-proof-v1'
    assert r8_r9['status'] == 'verified_source_delta'
    assert r8_r9['gate_status'] == 'not_assessed_by_source_proof'
    assert r8_r9['changed_paths'] == ['crates/runtime/src/headless/run.rs']
    assert sha(r8_r9_delta) == r8_r9['delta']['patch_sha256']
    for key, gate_name in (('previous_snapshot', 'r8'), ('current_snapshot', 'r9')):
        snapshot = r8_r9[key]
        source_patch = copied_reference(snapshot['source_patch'])
        current_patch = gate_snapshots[gate_name][1]
        assert snapshot['name'] == gate_name
        assert snapshot['source_patch_sha256'] == gate_snapshots[gate_name][2] == sha(source_patch)
        assert snapshot['source_current_patch_sha256'] == sha(current_patch)

    r9_base = (gate_dirs['r9'] / 'candidate-head.txt').read_text(encoding='utf-8').strip()
    r10_base = (gate_dirs['r10'] / 'candidate-head.txt').read_text(encoding='utf-8').strip()
    assert re.fullmatch(r'[0-9a-f]{40}', r9_base) and r9_base == r10_base

    final = json.loads((HERE / SOURCE_PROOF_FILES[4]).read_text(encoding='utf-8'))
    final_delta = copied_reference(final['delta']['patch'])
    assert final['schema'] == 'r376-r8-final-single-production-file-proof-v1'
    assert final['status'] == 'verified_source_delta'
    assert final['gate_status'] == 'not_assessed_by_source_proof'
    assert final['changed_paths'] == ['crates/runtime/src/headless/run.rs']
    assert final['intermediate_deltas'] == {
        'r8_to_r9_changed_paths': ['crates/runtime/src/headless/run.rs'],
        'r9_to_final_changed_paths': ['crates/runtime/src/headless/run.rs'],
    }
    assert sha(final_delta) == final['delta']['patch_sha256']
    for snapshot, gate_name in (
        (final['previous_snapshot'], 'r8'),
        (final['intermediate_snapshots'][0], 'r9'),
        (final['current_snapshot'], 'r10'),
    ):
        source_patch = copied_reference(snapshot['source_patch'])
        assert snapshot['name'] == gate_name
        assert snapshot['source_patch_sha256'] == gate_snapshots[gate_name][2] == sha(source_patch)
        assert snapshot['source_current_patch_sha256'] == sha(gate_snapshots[gate_name][1])

    for gate_name in ('r9', 'r10'):
        directory = gate_dirs[gate_name]
        status = directory / 'status.txt'
        assert status.is_file() and status.read_text(encoding='utf-8').strip() == 'passed', status
        for phase in ('fmt', 'runtime-lib', 'changed-clippy'):
            exit_path = directory / f'{phase}.exit'
            log_path = directory / f'{phase}.log'
            assert exit_path.is_file() and exit_path.read_text(encoding='utf-8').strip() == '0', exit_path
            assert log_path.is_file() and not log_path.is_symlink(), log_path
            relative = str(log_path.relative_to(HERE))
            assert relative in copy_items, relative

    required = set(SOURCE_PROOF_FILES + SOURCE_PROOF_TOOLS)
    required.update(
        str(path.relative_to(HERE))
        for name in ('r6', 'r7', 'r8', 'r9', 'r10')
        for path in gate_dirs[name].glob('source*')
    )
    missing = sorted(required - set(copy_items))
    assert not missing, missing
    print(f'PASS: source artifact closure ({len(copy_items)} planned files; r6, r8-r9, r8-r10, r9/r10 gate evidence)')


def final_tested_implementation(baseline):
    final = read('final-baseline.json')
    assert final['schema'] == 'claude-295-final-baseline-r376-v1'
    assert final['initial_baseline_sha256'] == sha(HERE / 'baseline.json')
    assert final['target_version'] == baseline['target_version'] == '2.1.295'
    assert final['native_sha256'] == baseline['native_sha256']
    tested = final['tested_implementation']
    assert set(tested) == set(baseline['baseline'])
    for repo, commit in tested.items():
        assert re.fullmatch(r'[0-9a-f]{40}', commit), (repo, commit)
    assert tested['llm-client'] == baseline['baseline']['llm-client']['head']
    for repo in ('harness-runtime', 'lingxi-app'):
        assert tested[repo] != baseline['baseline'][repo]['head'], (
            repo, 'final source repair must not retain the pre-repair tested SHA'
        )
    gate = final['repair_gate_receipt']
    receipt_path = (HERE / gate['path']).resolve()
    assert receipt_path.is_file(), receipt_path
    assert receipt_path.is_relative_to(HERE.resolve()), receipt_path
    assert sha(receipt_path) == gate['sha256'], receipt_path
    platform = read('platform-receipt.json')
    assert platform['status'] == 'passed'
    assert platform['candidate']['llm_client'] == tested['llm-client']
    assert platform['candidate']['harness_runtime'] == tested['harness-runtime']
    assert platform['candidate']['product'] == tested['lingxi-app']
    return tested, receipt_path


def native_alias_probe_accounting():
    attempts = []
    for name in (
        'native-ltr-alias-probe-run-once/receipt.json',
        'native-ltr-alias-probe-approved-endpoint-run/receipt.json',
    ):
        receipt = read(name)
        assert receipt['schema'] == 'native-2.1.295-ltr-alias-probe-v1', name
        count = receipt['native_process_count']
        assert isinstance(count, int) and count >= 0, (name, count)
        assert receipt['native_process_started'] == (count > 0), (name, receipt)
        attempts.append({
            'receipt': name,
            'status': receipt['status'],
            'native_process_count': count,
        })
    return attempts, sum(attempt['native_process_count'] for attempt in attempts), read(attempts[-1]['receipt'])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check-source-artifact-closure', action='store_true',
                        help='validate proof references and the source-artifact copy plan without assembling')
    args = parser.parse_args()
    if args.check_source_artifact_closure:
        check_source_artifact_closure()
        return

    check_source_artifact_closure()
    baseline = read('baseline.json')
    tested, repair_receipt_path = final_tested_implementation(baseline)
    ownership = read('ownership.json')
    reports = {name: read(name + '-audit.json') for name in ('sdk', 'runtime', 'mods', 'gates')}
    native_attempts, native_launch_count, native_probe = native_alias_probe_accounting()
    DEST.mkdir(parents=True, exist_ok=True)
    names = ['.gitattributes', 'baseline.json', 'ownership.json', 'ledger-rules.md', 'platform-receipt.json',
             'run-platform-checks.sh', 'platform-run.log', 'hook-corpus.log',
             'source-oracle-receipt.json', 'source-oracle-differential.mjs',
             'source-oracle.log', 'oracle-differential-results.jsonl',
             'sdk-message-controls.log', 'sdk-message-controls-receipt.json',
             'final-baseline.json', 'hooks-comma-receipt.json',
             'validate-ledger.py', 'assemble-ledger.py', 'checkpoint-current.md', 'matrix-introduction.md',
             'native-ltr-alias-probe-plan.md',
             'independent-finalizer-review.md', 'independent-cli-sigint-review.md',
             'cancellation-terminal-review.md', 'cancellation-terminal-review.json',
             'make-mac-aggregate-r376.py', 'mac-aggregate-r376-runbook.md',
             'canonical-before.json', 'canonical-preservation-final-17593.json']
    names += list(SOURCE_PROOF_TOOLS)
    names += list(SOURCE_PROOF_FILES)
    names += [owner + '-audit.' + suffix for owner in reports for suffix in ('md', 'json')]
    names += ['platform/' + path.name for path in sorted((HERE / 'platform').glob('*.log'))]
    names += [str(path.relative_to(HERE)) for path in sorted((HERE / 'receipts').rglob('*.json'))]
    repair_receipt = json.loads(repair_receipt_path.read_text())
    assert repair_receipt['schema'] == 'claude-295-repairs-gate-receipt-r1'
    assert repair_receipt['status'] == 'passed'
    assert repair_receipt['repos'] == {
        'sdk': tested['llm-client'],
        'harness': tested['harness-runtime'],
        'product': tested['lingxi-app'],
    }
    package_path = (HERE / repair_receipt['package']['receipt']).resolve()
    acceptance_root = (HERE.parent).resolve()
    assert package_path.is_file(), package_path
    assert package_path.is_relative_to(acceptance_root), package_path
    copy_items = {name: HERE / name for name in names}
    copy_items['repairs-gate-receipt.json'] = repair_receipt_path
    copy_items[str(repair_receipt_path.relative_to(HERE.resolve()))] = repair_receipt_path
    copy_items['mac-package-r376-receipt.json'] = package_path
    for path in sorted((HERE / 'mac-package-evidence-17593').rglob('*')):
        if path.is_file():
            copy_items[str(path.relative_to(HERE))] = path
    for path in sorted((HERE / 'final-evidence-17593-v2').rglob('*')):
        if path.is_file():
            copy_items[str(path.relative_to(HERE))] = path
    hook_receipt = read('hooks-comma-receipt.json')
    for name in hook_receipt['logs']:
        copy_items[name] = HERE / name
    for repair_files in sorted(HERE.glob('harness-repair-gates*')):
        if repair_files.is_dir():
            for path in sorted(repair_files.rglob('*')):
                if path.is_file():
                    copy_items[str(path.relative_to(HERE))] = path
    for phase in repair_receipt['phases']:
        log_path = (HERE / phase['log']).resolve()
        assert log_path.is_file(), log_path
        assert log_path.is_relative_to(HERE.resolve()), log_path
        copy_items[str(log_path.relative_to(HERE.resolve()))] = log_path
    for attempt in native_attempts:
        receipt_path = HERE / attempt['receipt']
        if receipt_path.parent.name == 'native-ltr-alias-probe-run-once':
            copy_items[attempt['receipt']] = receipt_path
        else:
            output_dir = receipt_path.parent
            for path in sorted(output_dir.rglob('*')):
                if path.is_file():
                    copy_items[str(path.relative_to(HERE.resolve()))] = path
    artifacts = []
    for source_name, src in copy_items.items():
        dst = DEST / source_name
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(src, dst)
        artifacts.append({'path': str(dst.relative_to(DOC)), 'sha256': hashlib.sha256(dst.read_bytes()).hexdigest()})
    lookup = {}
    for owner in ('sdk', 'runtime', 'mods'):
        rows = reports[owner].get('contracts', reports[owner].get('rows'))
        assert isinstance(rows, list), (owner, reports[owner].keys())
        for row in rows:
            assert row['id'] not in lookup, row['id']
            lookup[row['id']] = row
    contracts = []
    for original in baseline['contracts']:
        contract = dict(original)
        owner = next(name for name, ids in ownership.items() if contract['id'] in ids)
        contract['owner'] = owner
        if owner in ('sdk', 'runtime', 'mods'):
            contract['assessment'] = lookup[contract['id']]
            contract['audit'] = 'evidence/final-ledger-r376/' + owner + '-audit.json'
        elif owner == 'gates':
            contract['assessment'] = {'status': 'verified_build_and_client_regression_scoped',
                'evidence': ['gates-audit.json', 'platform-receipt.json',
                    'repairs-gate-receipt.json', 'mac-package-r376-receipt.json'],
                'limitations': ['The named Rust and macOS package checks bind to final-baseline.json; they do not establish generated-language consumer integration or physical-device behavior. Cross-target compile is not AAR/XCFramework runtime acceptance. No claim of every client output byte matching Native.']}
            contract['audit'] = 'evidence/final-ledger-r376/gates-audit.json'
        else:
            contract['assessment'] = {'status': 'verified_frozen_target', 'evidence': ['baseline.json'],
                'limitations': ['Version freeze is verified; it does not itself establish all contract parity.']}
            contract['audit'] = 'evidence/final-ledger-r376/baseline.json'
        contracts.append(contract)
    ledger = {'schema': 'claude-295-final-evidence-ledger-r376-v1', 'target_version': '2.1.295',
        'native_sha256': baseline['native_sha256'],
        'tested_implementation': tested,
        'status': '42 contracts reconciled against explicit final H/P source SHAs; consult per-row source and validation limits; not a universal byte-parity certification',
        'new_native_launches': native_launch_count,
        'native_launch_accounting': {
            'scope': 'The two one-shot Native 2.1.295 alias/glob probe attempts in this cleanup batch; historical fixture captures are not counted.',
            'attempts': native_attempts,
            'interpretation': 'Counts started Native processes from each runner receipt, including invalid captures; invalid attempts do not establish alias/glob semantics.',
        },
        'native_alias_probe_semantics': {
            'status': native_probe['status'],
            'evidence': ['native-ltr-alias-probe-plan.md', 'native-ltr-alias-probe-approved-endpoint-run/receipt.json'],
            'limitation': native_probe['interpretation'],
        },
        'contract_count': 42,
        'documentation_commit_policy': 'Documentation commits do not repin unchanged Product dependencies or rebuild the accepted signed package. Tested implementation SHAs stay explicit.',
        'contracts': contracts, 'audit_artifacts': artifacts}
    (DOC / 'final-ledger-r376.json').write_text(json.dumps(ledger, ensure_ascii=False, indent=2) + '\n')
    rows = [f"| {row['id']} | {row['assessment']['status']} | [{row['owner']}]({row['audit']}) |"
            for row in contracts]
    (DOC / 'current-matrix.md').write_text(
        (HERE / 'matrix-introduction.md').read_text().rstrip() + '\n' + '\n'.join(rows) + '\n')
    print('assembled 42-row ledger and hash-bound audit artifacts')


if __name__ == '__main__':
    main()
