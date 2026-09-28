#!/usr/bin/env python3
"""Checked-in Phase 7 Local App MCP contract gate.

This source gate complements the Rust tests with cheap checks for the
security-sensitive seams that must not silently regress: connection-scoped
identity, one physical hub, active-catalog freshness, bounded exposure, and
redacted/cancellable calls.
"""

from __future__ import annotations

import json
import re
from pathlib import Path


REPO = Path(__file__).resolve().parents[2]
REGISTRY = REPO / "crates" / "mcp" / "src" / "registry.rs"
TRANSPORT = REPO / "crates" / "harness-runtime" / "src" / "mobile" / "local_apps_mcp.rs"
TASKS = REPO / "docs" / "local-apps" / "harness" / "tasks-phase-7.json"


def fail(message: str) -> None:
    raise SystemExit(f"PHASE7-PLUGIN FAIL: {message}")


def require(text: str, needle: str, label: str) -> None:
    if needle not in text:
        fail(f"{label} is missing {needle!r}")


def main() -> None:
    if not TASKS.is_file():
        fail("checked-in Phase 7 task contract is missing")
    try:
        contract = json.loads(TASKS.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        fail(f"Phase 7 task contract is invalid JSON: {error}")
    if contract.get("phase") != "7" or [task.get("id") for task in contract.get("tasks", [])] != [
        "P7.0",
        "P7.1",
        "P7.2",
        "P7.3",
    ]:
        fail("task contract must contain P7.0 through P7.3 in order")

    registry = REGISTRY.read_text(encoding="utf-8")
    for needle in [
        "pub struct ConversationExport",
        "local_apps:conversation-export:",
        "mcp__{}__{}",
        "pub struct ManagedLocalAppServer",
        "physical_transport_count",
        "register_managed_local_app",
        "actual_surface_changed",
        "pub struct LocalAppExposure",
        "LOCAL_APP_MAX_EXPOSED: usize = 8",
        "LOCAL_APP_MAX_IN_FLIGHT_PER_APP: usize = 4",
        "exposure_capacity_reached",
        "pub async fn begin_local_app_call",
    ]:
        require(registry, needle, "McpRegistry Phase 7 seam")
    if not re.search(r"format!\(\"local_app_\{\}\", self\.app_id\)", registry):
        fail("logical server identity must preserve the raw App ID")
    if "server.scope.listed_tool_surface_sha256" not in registry:
        fail("surface notifications must compare committed digests, not a caller hint")

    transport = TRANSPORT.read_text(encoding="utf-8")
    for needle in [
        "ConversationExport(",
        "execute_mcp_flow",
        "tool_surface_stale",
        "rate_limited",
        "LOCAL_APP_CALL_TIMEOUT",
        "input.get(\"app_id\")",
        "validate_generated_structured_result",
        "tools.listChanged",
        "LocalAppAuditEntry",
        "input_sha256",
    ]:
        require(transport, needle, "LocalAppsMcpTransport Phase 7 seam")
    if "registry_key == LOCAL_APPS_REGISTRY_KEY" not in transport or "scope.registry_key()" not in transport:
        fail("global and per-App in-process registry keys must be explicit")
    if "serde_json::to_vec(&request)" not in transport:
        fail("audit input must be represented by a digest, not persisted payload")

    print("PHASE7-PLUGIN OK: scoped identity, one hub, active catalog, bounded exposure and Host-only calls")


if __name__ == "__main__":
    main()
