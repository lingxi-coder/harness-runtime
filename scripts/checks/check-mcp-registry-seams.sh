#!/usr/bin/env bash
# Wrapper matching the check-phase2-plugin.sh / check-all.sh naming convention
# (`check-*.sh`). Without it, scripts/check-all.sh's `for f in scripts/check-*.sh
# scripts/*-gate.sh` glob never discovers scripts/checks/check-mcp-registry-seams.py, and
# scripts/tests/test_gate_triggers.py's own meta-gate — written to catch exactly a
# checked-in gate script with zero automation triggers — is blind to it too,
# because that meta-gate also only enumerates `check-*.sh` / `*-gate.sh`.
set -euo pipefail
cd "$(dirname "$0")/../.."
python3 scripts/checks/check-mcp-registry-seams.py
