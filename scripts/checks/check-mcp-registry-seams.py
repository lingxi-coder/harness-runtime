#!/usr/bin/env python3
"""Source gate for the MCP registry's managed-server seams.

It complements the Rust tests with cheap checks for seams that must not silently regress: one physical hub,
committed-digest freshness notifications, and bounded exposure. The scoped identity and the in-process
transport belong to the Local App repository and are checked there.
"""

from __future__ import annotations

from pathlib import Path


REPO = Path(__file__).resolve().parents[2]
REGISTRY = REPO / "crates" / "mcp" / "src" / "registry.rs"
REGISTRY_MANAGED = REPO / "crates" / "mcp" / "src" / "registry" / "managed.rs"


def fail(message: str) -> None:
    raise SystemExit(f"MCP-REGISTRY-SEAMS FAIL: {message}")


def require(text: str, needle: str, label: str) -> None:
    if needle not in text:
        fail(f"{label} is missing {needle!r}")


def main() -> None:
    registry_root = REGISTRY.read_text(encoding="utf-8")
    require(registry_root, "mod managed;", "McpRegistry module wiring")
    registry = registry_root + "\n" + REGISTRY_MANAGED.read_text(encoding="utf-8")
    for needle in [
        "pub struct ManagedServer",
        "physical_transport_count",
        "register_managed_server",
        "actual_surface_changed",
        "pub struct ServerExposure",
        "MAX_EXPOSED_SERVERS: usize = 8",
        "MAX_IN_FLIGHT_PER_SERVER: usize = 4",
        "exposure_capacity_reached",
        "pub async fn begin_server_call",
    ]:
        require(registry, needle, "McpRegistry managed-server seam")
    if "server.scope.listed_tool_surface_sha256" not in registry:
        fail("surface notifications must compare committed digests, not a caller hint")

    print("MCP-REGISTRY-SEAMS OK: one hub, committed-digest notifications, bounded exposure")


if __name__ == "__main__":
    main()
