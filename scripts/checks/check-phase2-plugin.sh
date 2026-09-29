#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
python3 scripts/checks/check-phase2-plugin.py

if ! contract_output="$(cargo test --locked -q -p harness-runtime --features mobile --features uniffi contract_digest 2>&1)"; then
    printf '%s\n' "$contract_output" >&2
    echo "PHASE2-CONTRACT FAIL: runtime profile catalog must match production and the pre-release r1 golden" >&2
    exit 1
fi
echo "PHASE2-CONTRACT OK: all five catalog digests match production and tampering each family is rejected"

for workflow_test in \
    every_checked_in_plugin_workflow_passes_the_runtime_validators \
    phase4_and_phase6_workflows_use_real_orchestration \
    unified_build_workflow_executes_create_identity_chain_with_hermetic_agents
do
    if ! workflow_output="$(cargo test --locked -q -p workflow --test plugin_workflow_scripts "$workflow_test" 2>&1)"; then
        printf '%s\n' "$workflow_output" >&2
        echo "PHASE2-WORKFLOW FAIL: $workflow_test" >&2
        exit 1
    fi
done
echo "PHASE2-WORKFLOW OK: runtime validators, Phase4 orchestration, and Phase6 fail-closed execution"
