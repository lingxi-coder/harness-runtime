#!/usr/bin/env bash
# The engine-side half of the plugin's contract: the engine's workflow runtime validators and orchestration
# run the Local App plugin's checked-in workflow scripts (read through `local-app-plugin`). The plugin's own
# content is checked in its repository (scripts/checks/check-phase2-plugin.sh there).
set -euo pipefail
cd "$(dirname "$0")/../.."

for workflow_test in \
    every_checked_in_plugin_workflow_passes_the_runtime_validators \
    phase4_and_phase6_workflows_use_real_orchestration \
    unified_build_workflow_executes_create_identity_chain_with_hermetic_agents
do
    if ! workflow_output="$(cargo test --locked -q -p workflow --test plugin_workflow_scripts "$workflow_test" 2>&1)"; then
        printf '%s\n' "$workflow_output" >&2
        echo "PLUGIN-WORKFLOW FAIL: $workflow_test" >&2
        exit 1
    fi
done
echo "PLUGIN-WORKFLOW OK: runtime validators, orchestration, and fail-closed execution"
