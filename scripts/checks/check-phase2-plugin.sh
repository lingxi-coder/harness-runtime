#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
python3 scripts/checks/check-phase2-plugin.py

# The catalog digests are checked by the tests that live with the runtime profiles,
# which are in local-app-service (they were in harness-runtime until the profiles
# moved there; a `contract_digest` filter over harness-runtime then selected nothing
# and this step kept passing). A filter that matches nothing reports "0 passed" and
# exits 0, so the step also requires the tests to have actually run.
if ! contract_output="$(cargo test --locked -q -p local-app-service contract_digest 2>&1)"; then
    printf '%s\n' "$contract_output" >&2
    echo "PHASE2-CONTRACT FAIL: runtime profile catalog must match production and the pre-release r1 golden" >&2
    exit 1
fi
if ! grep -Eq 'test result: ok\. ([3-9]|[1-9][0-9]+) passed' <<<"$contract_output"; then
    printf '%s\n' "$contract_output" >&2
    echo "PHASE2-CONTRACT FAIL: fewer than three contract_digest tests ran in local-app-service" >&2
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
