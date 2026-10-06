#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
python3 scripts/checks/check-phase2-plugin.py

# The catalog-digest tests belong to local-app-service and run in its own repository's CI
# (scripts/checks/check-contract-digests.sh there): a `-p local-app-service` filter cannot reach a package that is
# not a member of this workspace.

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
