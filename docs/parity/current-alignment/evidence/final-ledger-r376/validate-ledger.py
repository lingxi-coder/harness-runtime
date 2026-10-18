"""Validate the bounded ledger and the final H/P source-repair gate receipts."""
import hashlib
import json
import re
import subprocess
from pathlib import Path

HERE = Path(__file__).resolve().parent
BASE = next(path for path in HERE.parents if (path / 'harness-runtime').is_dir() and (path / 'lingxi-app').is_dir())
DOC = BASE / 'harness-runtime/docs/parity/current-alignment'
REPAIR_RECEIPT_SCHEMA = 'claude-295-repairs-gate-receipt-r1'
FINAL_BASELINE_SCHEMA = 'claude-295-final-baseline-r376-v1'
LEDGER_SCHEMA = 'claude-295-final-evidence-ledger-r376-v1'
LEDGER_STATUS = (
    '42 contracts reconciled against explicit final H/P source SHAs; consult per-row source and validation limits; '
    'not a universal byte-parity certification'
)
REQUIRED_PHASES = {
    ('harness-runtime', 'fmt'),
    ('harness-runtime', 'permission-lib'),
    ('harness-runtime', 'cancel-inflight'),
    ('harness-runtime', 'orchestrator-lib'),
    ('harness-runtime', 'runtime-lib'),
    ('harness-runtime', 'agent-lib'),
    ('harness-runtime', 'tasks-lib'),
    ('harness-runtime', 'changed-clippy'),
    ('lingxi-app', 'skill-cold'),
    ('lingxi-app', 'product-source'),
    ('lingxi-app', 'product-check'),
    ('lingxi-app', 'product-clippy'),
}
DOC_PREFIXES = (
    'docs/',
    'acceptance/',
    'apps/web/src/data/docs/',
)
DOC_EXACT_PATHS = {
    'apps/web/public/api-index.json',
    'apps/web/src/data/rustdoc-manifest.json',
}
DOC_SUFFIXES = {'.md', '.mdx', '.rst'}
ALLOWED_H_SOURCE_PATHS = {
    'crates/core/src/host/mod.rs',
    'crates/core/src/host/permission_gate.rs',
    'crates/agent/src/handle.rs',
    'crates/agent/src/handle/spawn_context.rs',
    'crates/agent/src/handle/tests/tests.rs',
    'crates/agent/src/tool_resolver.rs',
    'crates/tasks/src/handlers/in_process_teammate.rs',
    'crates/tasks/src/handlers/in_process_teammate_test.rs',
    'crates/hooks/src/matcher.rs',
    'crates/hooks/src/registry.rs',
    'crates/orchestrator/src/conversation.rs',
    'crates/orchestrator/src/conversation/model.rs',
    'crates/orchestrator/src/conversation/drivers/mod.rs',
    'crates/orchestrator/src/conversation/drivers/projected_content.rs',
    'crates/orchestrator/src/conversation/drivers/streaming.rs',
    'crates/orchestrator/src/conversation/model_reminders.rs',
    'crates/orchestrator/src/conversation/reminders.rs',
    'crates/orchestrator/src/conversation/tests/turn_admission_cancel_tests.rs',
    'crates/orchestrator/src/conversation/tooling.rs',
    'crates/orchestrator/src/conversation/transcript.rs',
    'crates/orchestrator/src/conversation/wiring.rs',
    'crates/orchestrator/src/streaming_executor_test.rs',
    'crates/orchestrator/src/turn_loop_test.rs',
    'crates/orchestrator/tests/current_alignment_cancel.rs',
    'crates/orchestrator/tests/queued_prompt_batch_test.rs',
    'crates/permission/src/lib.rs',
    'crates/permission/src/policy.rs',
    'crates/permission/src/policy_gate.rs',
    'crates/permission/src/policy_gate_test.rs',
    'crates/permission/src/policy_test.rs',
    'crates/runtime/src/desktop/assembly.rs',
    'crates/runtime/src/desktop/configuration.rs',
    'crates/runtime/src/desktop/mod.rs',
    'crates/runtime/src/desktop/tests/tests.rs',
    'crates/runtime/src/mobile/host/assembly.rs',
    'crates/runtime/src/mobile/host/tests/bash_precommit_wiring_tests.rs',
    'crates/runtime/src/headless/run/control.rs',
    'crates/runtime/src/headless/run.rs',
}
ALLOWED_P_SOURCE_PATHS = {
    'Cargo.toml',
    'Cargo.lock',
    'apps/bridge-server/src/driver.rs',
    'apps/bridge-server/src/server.rs',
    'apps/cli/host/src/loop_wakeup.rs',
    'apps/cli/host/src/init.rs',
    'apps/cli/host/src/repl.rs',
    'apps/cli/host/tests/commands_native_cli.rs',
    'apps/cli/host/tests/fixtures/cli-cancel-native-295/README.md',
    'apps/cli/host/tests/fixtures/cli-cancel-native-295/stdout.raw',
    'apps/cli/host/tests/fixtures/cli-cancel-native-295/prompt-content.raw.json',
    'apps/cli/host/tests/fixtures/cli-cancel-native-295/interrupt-content.raw.json',
}
PRODUCT_FIXTURE_PATHS = {
    'apps/cli/host/tests/fixtures/cli-cancel-native-295/README.md',
    'apps/cli/host/tests/fixtures/cli-cancel-native-295/stdout.raw',
    'apps/cli/host/tests/fixtures/cli-cancel-native-295/prompt-content.raw.json',
    'apps/cli/host/tests/fixtures/cli-cancel-native-295/interrupt-content.raw.json',
}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def git(repo, *args):
    return subprocess.check_output(['git', *args], cwd=BASE / repo, text=True).strip()


def git_bytes(repo, *args):
    return subprocess.check_output(['git', *args], cwd=BASE / repo)


def diff_sections(patch, label):
    sections = {}
    chunks = re.split(rb'(?=^diff --git a/)', patch, flags=re.MULTILINE)
    for chunk in chunks:
        if not chunk:
            continue
        match = re.match(rb'diff --git a/(.*?) b/(.*?)\n', chunk)
        assert match, f'{label} contains bytes outside a standard diff section'
        old_path, new_path = (value.decode('utf-8') for value in match.groups())
        assert old_path == new_path, (label, old_path, new_path)
        assert old_path not in sections, (label, old_path)
        sections[old_path] = chunk
    return sections


def read_json(path):
    return json.loads(path.read_text())


def resolve_acceptance_path(raw, anchor=HERE):
    path = Path(raw)
    assert not path.is_absolute(), raw
    resolved = (anchor / path).resolve()
    assert resolved.is_relative_to((BASE / 'acceptance').resolve()), resolved
    assert resolved.is_file(), resolved
    return resolved


def is_document_path(path):
    if path in PRODUCT_FIXTURE_PATHS:
        return False
    return path.startswith(DOC_PREFIXES) or path in DOC_EXACT_PATHS or Path(path).suffix.lower() in DOC_SUFFIXES


def source_delta(repo, old_head, new_head):
    changed = git(repo, 'diff', '--name-only', f'{old_head}..{new_head}', '--').splitlines()
    return sorted(path for path in changed if not is_document_path(path))


def render_expected_contract_rows(baseline, ownership, reports):
    """Mirror assemble-ledger.py so each assessment is checked against its source row."""
    owner_rows = {}
    for owner in ('sdk', 'runtime', 'mods'):
        rows = reports[owner].get('contracts', reports[owner].get('rows'))
        assert isinstance(rows, list), (owner, reports[owner].keys())
        for row in rows:
            assert row['id'] not in owner_rows, row['id']
            owner_rows[row['id']] = row

    rendered = []
    for original in baseline['contracts']:
        contract = dict(original)
        contract_id = contract['id']
        owner = next(name for name, ids in ownership.items() if contract_id in ids)
        contract['owner'] = owner
        if owner in ('sdk', 'runtime', 'mods'):
            contract['assessment'] = owner_rows[contract_id]
            contract['audit'] = f'evidence/final-ledger-r376/{owner}-audit.json'
        elif owner == 'gates':
            contract['assessment'] = {
                'status': 'verified_build_and_client_regression_scoped',
                'evidence': [
                    'gates-audit.json', 'platform-receipt.json',
                    'repairs-gate-receipt.json', 'mac-package-r376-receipt.json',
                ],
                'limitations': [
                    'The named Rust and macOS package checks bind to final-baseline.json; they do not establish generated-language consumer integration or physical-device behavior. Cross-target compile is not AAR/XCFramework runtime acceptance. No claim of every client output byte matching Native.'
                ],
            }
            contract['audit'] = 'evidence/final-ledger-r376/gates-audit.json'
        else:
            contract['assessment'] = {
                'status': 'verified_frozen_target',
                'evidence': ['baseline.json'],
                'limitations': ['Version freeze is verified; it does not itself establish all contract parity.'],
            }
            contract['audit'] = 'evidence/final-ledger-r376/baseline.json'
        rendered.append(contract)
    return rendered


def validate_rendered_contract_rows(ledger_rows, baseline, ownership, reports):
    expected = render_expected_contract_rows(baseline, ownership, reports)
    assert isinstance(ledger_rows, list) and len(ledger_rows) == len(expected), (
        'ledger contract row count differs from assembler output', len(ledger_rows) if isinstance(ledger_rows, list) else type(ledger_rows).__name__, len(expected)
    )
    for index, (actual, wanted) in enumerate(zip(ledger_rows, expected)):
        if actual != wanted:
            keys = set(actual) | set(wanted) if isinstance(actual, dict) else set()
            differing = sorted(key for key in keys if not isinstance(actual, dict) or actual.get(key) != wanted.get(key))
            contract_id = wanted.get('id', '<missing-id>')
            raise AssertionError(
                f'ledger contract row {index} ({contract_id}) differs from its rendered source; fields={differing}'
            )


def validate_ledger_header(ledger, expected_contract_count):
    assert ledger['schema'] == LEDGER_SCHEMA
    assert ledger['contract_count'] == expected_contract_count
    assert ledger['status'] == LEDGER_STATUS


def validate_harness_final_followup(receipt, baseline, harness_sha, h_delta, keyed_phases):
    binding = receipt['harness_source_binding']
    base_head = baseline['baseline']['harness-runtime']['head']
    path = 'crates/runtime/src/headless/run.rs'
    assert path in h_delta
    assert binding['baseline_head'] == base_head
    assert binding['final_head'] == harness_sha
    assert binding['r10_delta_path'] == path
    assert binding['final_commit_patch_matches_r8'] is False
    assert binding['final_commit_patch_matches_r9'] is False
    assert binding['final_commit_patch_matches_r10'] is True

    snapshots = binding['source_patch_snapshots']
    assert set(snapshots) == {'r6', 'r7', 'r8', 'r9', 'r10'}
    for name, snapshot in snapshots.items():
        candidate_path = HERE / f'harness-repair-gates-{name}' / 'candidate-head.txt'
        if candidate_path.exists() or candidate_path.is_symlink():
            assert candidate_path.is_file() and not candidate_path.is_symlink(), candidate_path
            expected_execution_head = candidate_path.read_text(encoding='utf-8').strip()
        else:
            expected_execution_head = base_head
        assert re.fullmatch(r'[0-9a-f]{40}', expected_execution_head), (name, expected_execution_head)
        assert snapshot['execution_head'] == expected_execution_head, (name, snapshot['execution_head'], expected_execution_head)
    r8_patch_path = resolve_acceptance_path(snapshots['r8']['tested_source_patch'])
    r8_current_path = resolve_acceptance_path(snapshots['r8']['current_patch'])
    r9_patch_path = resolve_acceptance_path(snapshots['r9']['tested_source_patch'])
    r9_current_path = resolve_acceptance_path(snapshots['r9']['current_patch'])
    r10_patch_path = resolve_acceptance_path(snapshots['r10']['tested_source_patch'])
    r10_current_path = resolve_acceptance_path(snapshots['r10']['current_patch'])
    r8_patch, r9_patch, r10_patch = r8_patch_path.read_bytes(), r9_patch_path.read_bytes(), r10_patch_path.read_bytes()
    assert sha(r8_patch_path) == snapshots['r8']['sha256'] == binding['r8_tested_source_patch_sha256']
    assert sha(r9_patch_path) == snapshots['r9']['sha256'] == binding['r9_intermediate_source_patch_sha256']
    assert sha(r10_patch_path) == snapshots['r10']['sha256'] == binding['r10_final_source_patch_sha256']
    for current_path, snapshot in ((r8_current_path, 'r8'), (r9_current_path, 'r9'), (r10_current_path, 'r10')):
        assert sha(current_path) == snapshots[snapshot]['current_patch_sha256']
    assert r8_current_path.read_bytes() == r8_patch
    assert r9_current_path.read_bytes() == r9_patch
    assert r10_current_path.read_bytes() == r10_patch

    candidate = binding['r10_candidate_head']
    assert re.fullmatch(r'[0-9a-f]{40}', candidate)
    assert candidate == snapshots['r9']['execution_head'] == snapshots['r10']['execution_head']
    subprocess.run(['git', 'merge-base', '--is-ancestor', candidate, harness_sha], cwd=BASE / 'harness-runtime', check=True)
    committed_r8 = git_bytes('harness-runtime', 'diff', '--binary', f'{base_head}..{candidate}', '--', 'crates', 'Cargo.toml', 'Cargo.lock')
    assert committed_r8 == r8_patch, 'r8 tested patch is not the exact committed pre-r9 H source'
    committed_final = git_bytes('harness-runtime', 'diff', '--binary', f'{base_head}..{harness_sha}', '--', 'crates', 'Cargo.toml', 'Cargo.lock')
    assert committed_final == r10_patch, 'final H patch is not the exact r10 tested source snapshot'
    assert source_delta('harness-runtime', candidate, harness_sha) == [path]

    r8_sections = diff_sections(r8_patch, 'r8 source patch')
    r9_sections = diff_sections(r9_patch, 'r9 intermediate source patch')
    r10_sections = diff_sections(r10_patch, 'r10 final source patch')
    def changed_paths(left, right):
        return sorted(name for name in set(left) | set(right) if left.get(name) != right.get(name))
    assert path not in r8_sections and path in r9_sections and path in r10_sections
    assert changed_paths(r8_sections, r9_sections) == [path]
    assert changed_paths(r9_sections, r10_sections) == [path]
    assert changed_paths(r8_sections, r10_sections) == [path]
    unchanged = {name: hashlib.sha256(section).hexdigest() for name, section in sorted(r8_sections.items()) if name != path}

    proof_ref = binding['final_r8_to_r10_source_proof']
    proof_path = resolve_acceptance_path(proof_ref)
    assert sha(proof_path) == binding['final_r8_to_r10_source_proof_sha256']
    proof = read_json(proof_path)
    assert proof['schema'] == 'r376-r8-final-single-production-file-proof-v1'
    assert proof['status'] == 'verified_source_delta'
    assert proof['gate_status'] == 'not_assessed_by_source_proof'
    assert proof['all_other_source_diff_sections_identical'] is True
    assert proof['changed_paths'] == [path]
    assert proof['intermediate_deltas'] == {
        'r8_to_r9_changed_paths': [path],
        'r9_to_final_changed_paths': [path],
    }
    assert proof['unchanged_source_section_sha256'] == unchanged
    assert proof['previous_snapshot']['name'] == 'r8'
    assert proof['previous_snapshot']['source_patch_sha256'] == snapshots['r8']['sha256']
    assert proof['intermediate_snapshots'] == [{
        'name': 'r9',
        'base_head': base_head,
        'candidate_head': candidate,
        'source_patch': snapshots['r9']['tested_source_patch'],
        'source_patch_sha256': snapshots['r9']['sha256'],
        'source_current_patch_sha256': snapshots['r9']['current_patch_sha256'],
    }]
    assert proof['current_snapshot']['name'] == 'r10'
    assert proof['current_snapshot']['source_patch_sha256'] == snapshots['r10']['sha256']
    assert proof['current_snapshot']['source_patch_sha256'] == binding['r10_final_source_patch_sha256']

    delta = proof['delta']
    assert delta['path'] == path
    delta_path = resolve_acceptance_path(delta['patch'])
    delta_bytes = delta_path.read_bytes()
    assert sha(delta_path) == delta['patch_sha256'] == binding['final_r8_to_r10_delta_patch_sha256']
    assert delta['patch'] == binding['final_r8_to_r10_delta_patch']
    previous_blob = git('harness-runtime', 'rev-parse', f'{candidate}:{path}')
    final_blob = git('harness-runtime', 'rev-parse', f'{harness_sha}:{path}')
    baseline_blob = git('harness-runtime', 'rev-parse', f'{base_head}:{path}')
    assert previous_blob == baseline_blob == delta['previous_git_blob'] == binding['r10_previous_run_git_blob']
    assert final_blob == delta['current_git_blob'] == binding['r10_final_run_git_blob']
    old_content = git_bytes('harness-runtime', 'show', f'{candidate}:{path}')
    final_content = git_bytes('harness-runtime', 'show', f'{harness_sha}:{path}')
    assert hashlib.sha256(old_content).hexdigest() == delta['previous_content_sha256']
    assert hashlib.sha256(final_content).hexdigest() == delta['current_content_sha256']
    assert delta['baseline_git_blob'] == baseline_blob
    final_delta = git_bytes('harness-runtime', 'diff', '--binary', '--full-index', f'{candidate}..{harness_sha}', '--', path)
    assert final_delta == delta_bytes
    normalized_section = re.sub(
        rb'(?m)^index [0-9a-f]+\.\.([0-9a-f]+)(.*)$',
        lambda match: b'index ' + previous_blob.encode('ascii') + b'..' + final_blob.encode('ascii') + match.group(2),
        r10_sections[path],
    )
    assert normalized_section == delta_bytes

    assert binding['final_r10_phases'] == ['changed-clippy', 'fmt', 'runtime-lib']
    assert binding['r10_gate_status'] == 'harness-repair-gates-r10/status.txt'
    r10_status = resolve_acceptance_path(binding['r10_gate_status'])
    assert sha(r10_status) == binding['r10_gate_status_sha256'] and r10_status.read_text().strip() == 'passed'
    assert binding['r9_intermediate_gate_status'] == 'harness-repair-gates-r9/status.txt'
    r9_status = resolve_acceptance_path(binding['r9_intermediate_gate_status'])
    assert sha(r9_status) == binding['r9_intermediate_gate_status_sha256'] and r9_status.read_text().strip() == 'passed'
    r9_phases = binding['r9_intermediate_phases']
    assert {phase['name'] for phase in r9_phases} == {'fmt', 'runtime-lib', 'changed-clippy'}
    for phase in r9_phases:
        assert phase['status'] == 'passed_intermediate' and phase['exit_code'] == 0
        assert phase['source_patch_sha256'] == snapshots['r9']['sha256']
        log_path = resolve_acceptance_path(phase['log'])
        exit_path = resolve_acceptance_path(phase['exit_receipt'])
        assert sha(log_path) == phase['log_sha256']
        assert sha(exit_path) == phase['exit_receipt_sha256'] and exit_path.read_text().strip() == '0'

    expected_origins = {
        'permission-lib': 'r6', 'agent-lib': 'r6', 'tasks-lib': 'r6', 'cancel-inflight': 'r6',
        'orchestrator-lib': 'r6', 'runtime-lib': 'r10', 'changed-clippy': 'r10', 'fmt': 'r10',
        'changed-docs': 'r8', 'hooks-lib': 'r8', 'hooks-focused': 'r8',
    }
    r6_followup_sha = binding['test_only_followup_proof_sha256']
    r8_final_followup_sha = binding['final_r8_to_r10_source_proof_sha256']
    for name, origin in expected_origins.items():
        phase = keyed_phases[('harness-runtime', name)]
        assert phase['execution_gate_snapshot'] == origin, (name, phase.get('execution_gate_snapshot'))
        assert phase['execution_head'] == snapshots[origin]['execution_head'], (name, phase.get('execution_head'))
        assert phase['tested_source_patch_sha256'] == snapshots[origin]['sha256'], name
        assert phase['followup_source_closure_proof_sha256'] == r6_followup_sha, name
        assert phase['production_followup_source_closure_proof_sha256'] == r8_final_followup_sha, name
    for name in ('fmt', 'runtime-lib', 'changed-clippy'):
        phase = keyed_phases[('harness-runtime', name)]
        assert phase['exit_code'] == 0 and phase['execution_gate_snapshot'] == 'r10', name


def current_source_tree_is_at(repo, tested_head):
    head = git(repo, 'rev-parse', 'HEAD')
    subprocess.run(['git', 'merge-base', '--is-ancestor', tested_head, 'HEAD'], cwd=BASE / repo, check=True)
    later = git(repo, 'diff', '--name-only', f'{tested_head}..HEAD', '--').splitlines()
    staged = git(repo, 'diff', '--cached', '--name-only', '--').splitlines()
    unstaged = git(repo, 'diff', '--name-only', '--').splitlines()
    unexpected = [path for path in later + staged + unstaged if not is_document_path(path)]
    assert not unexpected, (repo, head, sorted(set(unexpected)))
    return head


def validate_final_baseline(baseline):
    final_path = HERE / 'final-baseline.json'
    final = read_json(final_path)
    assert final['schema'] == FINAL_BASELINE_SCHEMA
    assert final['initial_baseline_sha256'] == sha(HERE / 'baseline.json')
    assert final['target_version'] == baseline['target_version'] == '2.1.295'
    assert final['native_sha256'] == baseline['native_sha256']
    assert final['native_sha256'] == sha(Path('/Users/luolingfeng/.local/share/claude/versions/2.1.295'))

    tested = final['tested_implementation']
    assert set(tested) == set(baseline['baseline'])
    for repo, commit in tested.items():
        assert re.fullmatch(r'[0-9a-f]{40}', commit), (repo, commit)
    for repo, key in (('llm-client', 'llm-client'), ('harness-runtime', 'harness-runtime'), ('lingxi-app', 'lingxi-app')):
        assert git(repo, 'cat-file', '-e', tested[key] + '^{commit}') == ''
    assert tested['llm-client'] == baseline['baseline']['llm-client']['head']
    for repo in ('harness-runtime', 'lingxi-app'):
        assert tested[repo] != baseline['baseline'][repo]['head'], (
            repo, 'final source repair must use a new tested commit, not the original SHA'
        )

    repair = final['repair_gate_receipt']
    repair_path = resolve_acceptance_path(repair['path'])
    assert sha(repair_path) == repair['sha256'], repair_path
    receipt = read_json(repair_path)
    assert receipt['schema'] == REPAIR_RECEIPT_SCHEMA
    assert receipt['status'] == 'passed'
    repo_pins = receipt['repos']
    assert repo_pins == {
        'sdk': tested['llm-client'],
        'harness': tested['harness-runtime'],
        'product': tested['lingxi-app'],
    }, repo_pins

    expected_delta = {
        'harness': source_delta('harness-runtime', baseline['baseline']['harness-runtime']['head'], tested['harness-runtime']),
        'product': source_delta('lingxi-app', baseline['baseline']['lingxi-app']['head'], tested['lingxi-app']),
    }
    assert expected_delta['harness'], 'final H commit has no source repair delta'
    assert expected_delta['product'], 'final P commit has no source repair delta'
    assert receipt['source_delta'] == expected_delta, (receipt['source_delta'], expected_delta)
    h_delta = set(expected_delta['harness'])
    p_delta = set(expected_delta['product'])
    required_h_sources = {
        'crates/core/src/host/mod.rs',
        'crates/core/src/host/permission_gate.rs',
        'crates/agent/src/handle.rs',
        'crates/agent/src/handle/spawn_context.rs',
        'crates/agent/src/handle/tests/tests.rs',
        'crates/agent/src/tool_resolver.rs',
        'crates/tasks/src/handlers/in_process_teammate.rs',
        'crates/tasks/src/handlers/in_process_teammate_test.rs',
        'crates/hooks/src/matcher.rs',
        'crates/hooks/src/registry.rs',
        'crates/permission/src/policy_gate_test.rs',
        'crates/orchestrator/tests/current_alignment_cancel.rs',
        'crates/orchestrator/src/conversation/model.rs',
        'crates/runtime/src/mobile/host/assembly.rs',
        'crates/runtime/src/mobile/host/tests/bash_precommit_wiring_tests.rs',
        'crates/runtime/src/headless/run/control.rs',
        'crates/runtime/src/headless/run.rs',
    }
    assert required_h_sources <= h_delta, required_h_sources - h_delta
    assert h_delta <= ALLOWED_H_SOURCE_PATHS, h_delta - ALLOWED_H_SOURCE_PATHS
    assert any(path.startswith('crates/permission/') for path in h_delta), h_delta
    assert any(path.startswith('crates/orchestrator/src/') for path in h_delta), h_delta
    assert any(path.startswith('crates/runtime/src/desktop/') for path in h_delta), h_delta
    required_p_sources = {
        'apps/bridge-server/src/driver.rs',
        'apps/bridge-server/src/server.rs',
        'apps/cli/host/src/loop_wakeup.rs',
        'apps/cli/host/src/init.rs',
        'apps/cli/host/src/repl.rs',
        'apps/cli/host/tests/commands_native_cli.rs',
    } | PRODUCT_FIXTURE_PATHS
    assert required_p_sources <= p_delta, required_p_sources - p_delta
    assert p_delta <= ALLOWED_P_SOURCE_PATHS, p_delta - ALLOWED_P_SOURCE_PATHS

    phases = receipt['phases']
    keyed_phases = {}
    for phase in phases:
        key = (phase['repository'], phase['name'])
        assert key not in keyed_phases, key
        keyed_phases[key] = phase
        assert phase['repository'] in ('harness-runtime', 'lingxi-app'), key
        expected_head = tested[phase['repository']]
        assert phase['head'] == expected_head, (key, phase['head'], expected_head)
        assert phase['exit_code'] == 0, (key, phase['exit_code'])
        assert phase['command'].strip(), key
        log_path = resolve_acceptance_path(phase['log'])
        assert log_path.stat().st_size > 0, log_path
        assert sha(log_path) == phase['log_sha256'], log_path
    assert REQUIRED_PHASES <= set(keyed_phases), (REQUIRED_PHASES - set(keyed_phases))
    validate_harness_final_followup(receipt, baseline, tested['harness-runtime'], h_delta, keyed_phases)

    command = lambda key: keyed_phases[key]['command']
    assert 'cargo fmt' in command(('harness-runtime', 'fmt')) and '--check' in command(('harness-runtime', 'fmt'))
    for name, package in (
        ('permission-lib', 'permission'),
        ('orchestrator-lib', 'orchestrator'),
        ('runtime-lib', 'harness-runtime'),
        ('agent-lib', 'agent'),
        ('tasks-lib', 'tasks'),
    ):
        text = command(('harness-runtime', name))
        assert 'cargo test' in text and '--all-features' in text, (name, text)
        assert f'-p {package}' in text or f'--package {package}' in text, (name, text)
    cancel = command(('harness-runtime', 'cancel-inflight'))
    assert 'native_inflight_sigint_contract_reaches_orchestrator_output_and_jsonl' in cancel and '--exact' in cancel, cancel
    clippy = command(('harness-runtime', 'changed-clippy'))
    assert 'cargo clippy' in clippy and '--all-targets' in clippy and '-D warnings' in clippy, clippy
    cold = command(('lingxi-app', 'skill-cold'))
    assert 'cli_cold_resume_preserves_skill_source_and_normalizes_anthropic_wire' in cold and '--exact' in cold, cold
    product_source = command(('lingxi-app', 'product-source'))
    assert 'check-runtime-dependency.py' in product_source, product_source
    product_check = command(('lingxi-app', 'product-check'))
    assert 'cargo check' in product_check and '--workspace' in product_check and '--all-targets' in product_check and '--all-features' in product_check, product_check
    product_clippy = command(('lingxi-app', 'product-clippy'))
    assert 'cargo clippy' in product_clippy and '--workspace' in product_clippy and '--all-targets' in product_clippy and '-D warnings' in product_clippy, product_clippy

    hook = receipt['hook_gate']
    hook_path = resolve_acceptance_path(hook['receipt'])
    assert sha(hook_path) == hook['sha256'], hook_path
    hooks = read_json(hook_path)
    assert hooks['schema'] == 'r376-hook-comma-gate-v1'
    assert hooks['status'] == 'passed' and hooks['failed'] == 0 and hooks['focused_new_regression'] >= 1
    assert hooks['base_harness'] == baseline['baseline']['harness-runtime']['head']
    for name, digest in hooks['logs'].items():
        path = HERE / name
        assert path.is_file() and sha(path) == digest, path
    assert set(hooks['source_sha256']) == {
        'crates/hooks/src/matcher.rs', 'crates/hooks/src/registry.rs',
        'crates/core/src/host/mod.rs', 'crates/core/src/host/permission_gate.rs',
    }
    assert set(hooks['source_sha256']) <= h_delta
    for name, digest in hooks['source_sha256'].items():
        content = subprocess.check_output(['git', 'show', f"{tested['harness-runtime']}:{name}"], cwd=BASE / 'harness-runtime')
        assert hashlib.sha256(content).hexdigest() == digest, name

    package = receipt['package']
    package_path = resolve_acceptance_path(package['receipt'])
    assert sha(package_path) == package['sha256'], package_path
    package_receipt = read_json(package_path)
    assert package_receipt['status'] in ('passed', 'delivered'), package_receipt.get('status')
    pin_views = [package_receipt.get(key) for key in ('main_commits', 'tested_implementation', 'pins', 'candidate')]
    pin_aliases = {
        'llm-client': ('llm-client', 'llm_client', 'sdk'),
        'harness-runtime': ('harness-runtime', 'harness_runtime', 'harness'),
        'lingxi-app': ('lingxi-app', 'lingxi_app', 'product'),
    }
    package_pins = None
    for view in pin_views:
        if isinstance(view, dict) and all(any(alias in view for alias in aliases) for aliases in pin_aliases.values()):
            package_pins = {repo: next(view[alias] for alias in aliases if alias in view) for repo, aliases in pin_aliases.items()}
            break
    if package_pins is None and all(key in package_receipt for key in ('sdk_sha', 'harness_sha', 'product_sha')):
        package_pins = {
            'llm-client': package_receipt['sdk_sha'],
            'harness-runtime': package_receipt['harness_sha'],
            'lingxi-app': package_receipt['product_sha'],
        }
    assert package_pins == tested, (package_pins, tested)

    for repo in ('harness-runtime', 'lingxi-app'):
        current_source_tree_is_at(repo, tested[repo])
    assert git('llm-client', 'rev-parse', 'HEAD') == tested['llm-client']
    assert not git('llm-client', 'status', '--porcelain', '--untracked-files=no')

    product_manifest = (BASE / 'lingxi-app/Cargo.toml').read_text()
    product_lock = (BASE / 'lingxi-app/Cargo.lock').read_text()
    harness_sha = tested['harness-runtime']
    assert product_manifest.count(f'rev = "{harness_sha}"') == 39
    assert product_lock.count(f'?rev={harness_sha}#{harness_sha}') == 78
    return final, receipt, repair_path, package_path


def validate_native_alias_probe(final):
    attempt_paths = (
        'native-ltr-alias-probe-run-once/receipt.json',
        'native-ltr-alias-probe-approved-endpoint-run/receipt.json',
    )
    attempts = []
    for name in attempt_paths:
        receipt = read_json(HERE / name)
        assert receipt['schema'] == 'native-2.1.295-ltr-alias-probe-v1', name
        count = receipt['native_process_count']
        assert isinstance(count, int) and count >= 0, (name, count)
        assert receipt['native_process_started'] == (count > 0), (name, receipt)
        attempts.append({
            'receipt': name,
            'status': receipt['status'],
            'native_process_count': count,
        })
    assert final['new_native_launches'] == sum(item['native_process_count'] for item in attempts)
    accounting = final['native_launch_accounting']
    assert accounting['attempts'] == attempts
    assert 'invalid captures' in accounting['interpretation']

    probe = read_json(HERE / attempt_paths[-1])
    assert probe['status'] == 'capture_invalid'
    assert probe['native_process_count'] == 1
    assert probe['provider_request_count'] == 1
    assert probe['checks']['positive_edit_deny_observed'] is False
    assert 'do not infer alias/glob semantics' in probe['interpretation']
    assert all(item['canonical_tool_present_in_provider_catalog'] for item in probe['alias_observations'])
    semantics = final['native_alias_probe_semantics']
    assert semantics['status'] == probe['status']
    assert semantics['evidence'] == [
        'native-ltr-alias-probe-plan.md', attempt_paths[-1]
    ]
    assert semantics['limitation'] == probe['interpretation']
    return attempt_paths, probe


def main():
    baseline = read_json(HERE / 'baseline.json')
    final, repair, repair_path, package_path = validate_final_baseline(baseline)
    native_attempt_paths, native_probe = validate_native_alias_probe(final)
    ledger = read_json(DOC / 'final-ledger-r376.json')
    expected = [row['id'] for row in baseline['contracts']]
    validate_ledger_header(ledger, len(expected))
    actual = [row['id'] for row in ledger['contracts']]
    assert len(actual) == len(set(actual)) == 42
    assert actual == expected, (set(expected) - set(actual), set(actual) - set(expected))
    ownership = read_json(HERE / 'ownership.json')
    owner_reports = {name: read_json(HERE / f'{name}-audit.json') for name in ('sdk', 'runtime', 'mods')}
    validate_rendered_contract_rows(ledger['contracts'], baseline, ownership, owner_reports)
    assert ledger['target_version'] == final['target_version']
    assert ledger['native_sha256'] == final['native_sha256']
    assert ledger['tested_implementation'] == final['tested_implementation']
    gates_audit = read_json(HERE / 'gates-audit.json')
    repair_status = gates_audit['repair_candidate']
    assert repair_status['status'] == 'passed'
    assert repair_status['sdk'] == final['tested_implementation']['llm-client']
    assert repair_status['harness'] == final['tested_implementation']['harness-runtime']
    assert repair_status['product'] == final['tested_implementation']['lingxi-app']
    platform_status = gates_audit['platform']['repair_pin_recheck']
    assert platform_status['status'] == 'passed'
    assert platform_status['candidate'] == {
        'harness_runtime': final['tested_implementation']['harness-runtime'],
        'llm_client': final['tested_implementation']['llm-client'],
        'product': final['tested_implementation']['lingxi-app'],
    }
    artifact_paths = [item['path'] for item in ledger['audit_artifacts']]
    assert len(artifact_paths) == len(set(artifact_paths))
    required = {
        'baseline.json', 'final-baseline.json', 'repairs-gate-receipt.json',
        'hooks-comma-receipt.json', 'mac-package-r376-receipt.json',
        'sdk-audit.json', 'sdk-audit.md', 'runtime-audit.json', 'runtime-audit.md',
        'mods-audit.json', 'mods-audit.md', 'gates-audit.json', 'gates-audit.md',
        'platform-receipt.json', 'hook-corpus.log',
        'checkpoint-current.md', 'native-ltr-alias-probe-plan.md',
    }
    required_paths = {'evidence/final-ledger-r376/' + name for name in required}
    required_paths.add('evidence/final-ledger-r376/' + str(repair_path.relative_to(HERE.resolve())))
    hook_receipt = read_json(HERE / 'hooks-comma-receipt.json')
    for name in hook_receipt['logs']:
        required_paths.add('evidence/final-ledger-r376/' + name)
    for phase in repair['phases']:
        required_paths.add('evidence/final-ledger-r376/' + phase['log'])
        for key in ('stdout_log', 'exit_receipt'):
            if phase.get(key):
                required_paths.add('evidence/final-ledger-r376/' + phase[key])
    for phase_group in ('superseded_harness_phases', 'superseded_environment'):
        for phase in repair.get(phase_group, []):
            for key in ('log', 'exit_receipt'):
                if phase.get(key):
                    required_paths.add('evidence/final-ledger-r376/' + phase[key])

    source_binding = repair['harness_source_binding']
    for snapshot in source_binding['source_patch_snapshots'].values():
        for key in ('tested_source_patch', 'current_patch'):
            required_paths.add('evidence/final-ledger-r376/' + snapshot[key])
    for gate in repair['harness_gate_folders'].values():
        required_paths.add('evidence/final-ledger-r376/' + gate['source_patch'])
        if gate.get('status'):
            required_paths.add('evidence/final-ledger-r376/' + gate['status'])
    for key in (
        'test_only_followup_proof', 'test_only_delta_patch',
        'final_r8_to_r10_source_proof', 'final_r8_to_r10_delta_patch',
        'r9_intermediate_gate_status', 'r10_gate_status',
    ):
        required_paths.add('evidence/final-ledger-r376/' + source_binding[key])
    for phase in source_binding['r9_intermediate_phases']:
        for key in ('log', 'exit_receipt'):
            required_paths.add('evidence/final-ledger-r376/' + phase[key])
    for name in native_attempt_paths:
        required_paths.add('evidence/final-ledger-r376/' + name)
    for path in sorted((HERE / 'native-ltr-alias-probe-approved-endpoint-run').rglob('*')):
        if path.is_file():
            required_paths.add('evidence/final-ledger-r376/' + str(path.relative_to(HERE)))
    assert required_paths <= set(artifact_paths), (required_paths - set(artifact_paths))
    for item in ledger['audit_artifacts']:
        path = DOC / item['path']
        assert path.is_file(), path
        assert sha(path) == item['sha256'], path

    platform = read_json(HERE / 'platform-receipt.json')
    assert platform['status'] == 'passed'
    assert len(platform['checks']) == 5
    assert {item['name'] for item in platform['checks']} == {
        'android-arm64-default', 'android-x86_64-default',
        'android-arm64-computer-use', 'android-x86_64-computer-use', 'ios-simulator-default'}
    assert platform['candidate']['harness_runtime'] == final['tested_implementation']['harness-runtime']
    assert platform['candidate']['llm_client'] == final['tested_implementation']['llm-client']
    assert platform['candidate']['product'] == final['tested_implementation']['lingxi-app']
    for item in platform['checks']:
        assert item['exit_code'] == 0, item
        assert sha(BASE / item['log']) == item['log_sha256'], item['name']

    print('PASS: 42 original contracts; final H/P tested SHAs; exact source deltas; required H/P gates; hook/package pins; final-pin cross-target checks and evidence hashes')


if __name__ == '__main__':
    main()
