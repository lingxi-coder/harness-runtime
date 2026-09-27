#!/usr/bin/env python3
"""Small, checked-in Phase 6 contract gate.

This gate is deliberately source-oriented: it catches accidental reversion to
the Phase 2 MCP placeholder and schema-v2 identity before a Rust build starts.
Behavioral coverage remains in the local-apps/workflow test packages.
"""
from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CORE = ROOT / "crates" / "local-apps" / "src"
WORKFLOW = ROOT / "crates" / "plugins" / "lingxi-local-app" / "workflows" / "local-app-mcp-authoring.js"


def fail(message: str) -> None:
    raise SystemExit(f"PHASE6-PLUGIN FAIL: {message}")


def main() -> None:
    types = (CORE / "types.rs").read_text(encoding="utf-8")
    if not re.search(r"APPS_SCHEMA_VERSION:\s*u32\s*=\s*4", types):
        fail("APPS_SCHEMA_VERSION must be 4")
    ids = (CORE / "ids.rs").read_text(encoding="utf-8")
    if "APP_ID_MAX_LEN: usize = 54" not in ids or "{0,53}" not in ids:
        fail("app id grammar must be the v3 54-character form")
    manifest = (CORE / "manifest.rs").read_text(encoding="utf-8")
    for symbol in ["AppTemplateOrigin", "AppMcpCatalogRef", "template_origin", "active_mcp_catalog", "derive_publication_state", "active_state_corrupt"]:
        if symbol not in manifest:
            fail(f"manifest is missing {symbol}")
    authoring = (CORE / "mcp_authoring.rs").read_text(encoding="utf-8")
    for symbol in ["FlowValueBinding", "AppMcpFlowBinding", "AppMcpProposal", "McpCandidateJournal", "McpConfirmationReceipt", "approval_contract_sha256", "catalog_sha256"]:
        if symbol not in authoring:
            fail(f"MCP authoring core is missing {symbol}")
    workflow = WORKFLOW.read_text(encoding="utf-8")
    if "HOST_PRECONDITION_UNAVAILABLE" in workflow:
        fail("MCP authoring workflow is still a Phase 2 placeholder")
    if "mcp_authoring_required" not in workflow or "agent(" not in workflow:
        fail("MCP authoring workflow must preserve zero-tool handoff and agent orchestration")
    if "const EXTERNAL_KEYS = ['app_id', 'user_goal'];" not in workflow:
        fail("MCP authoring workflow external contract drifted")
    print("PHASE6-PLUGIN OK: schema v4, manifest pair guards, typed MCP authoring and workflow handoff")


if __name__ == "__main__":
    main()
