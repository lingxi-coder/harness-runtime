#!/usr/bin/env python3
"""M8-P14 dependency-graph gate — rule engine (driven by scripts/checks/check-deps.sh).

Reads `cargo metadata --no-deps` JSON on stdin and enforces the composition-root
dependency rules (design §8.1). See check-deps.sh for the rule summary. Exits
non-zero (with a violation list) if any crate depends in a forbidden direction.

`--list` prints the classification + workspace-dep map for debugging.
"""
import json
import os
import sys

EXEMPT = {"tools", "mock_stdio_mcp"}  # legacy monolith aggregator + test fixture
LEAVES = {"app", "example"}

# The abstraction crates must stay pure: they may not depend on any impl crate
# (tool/skill/command/platform/app) NOR on each other NOR on the monolith.
# This is the sharpest engine-tier invariant — a `tool-api` that learned about a
# concrete `tool-shell` would defeat the whole composition-root design.
API_CRATES = {"tool-api", "command-api"}

# Leaf libraries: they depend on nothing in the workspace but each other, so anything can name a camera, an MCP wire
# type or the rooted filesystem without compiling the engine. `core` re-exports them.
LEAF_LIBRARIES = {"device-api", "json-projection", "mcp-wire", "rooted-fs"}

# The crates that implemented Local App when it was a host feature, matched by NAME because an old git revision of
# the local-app-builder repository still resolves. That repository now ships only a plugin (skills and templates);
# nothing here may bring one of these crates back.
LOCAL_APP_CRATES = {"local-app-builder-contracts", "local-app-builder-plugin", "local-app-builder-service", "local-apps"}

# The SDK assembles existing components; components must never reach back into
# its product profiles or depend on a concrete UI to describe an interaction.
HARNESS_COMPONENT_CRATES = {"llm-runtime", "harness-runtime", "tool-api", "permission"}

# Source-preserving transport contracts are leaves of the runtime graph, not
# domain engines. `core` re-exports mcp-wire's DTOs/transport trait, so that one
# foundation edge is intentional. Keep the contract's own workspace edges
# pinned to the JSON projection layer; it must not gain an MCP client or other
# domain dependency.
SHARED_WIRE_CONTRACTS = {"mcp-wire": {"json-projection"}}
CORE_FOUNDATION_DEPS = {"branding", "jsonrpc"} | LEAF_LIBRARIES

# class -> forbidden dependee classes.
#
# Note the broad "engine -> {platform,command,skill}" prohibition is deliberately
# NOT enforced: peripheral engine-tier crates have legitimate edges (e.g. the
# `tui` palette depends on `command-core`). The high-signal invariants below —
# tool independence, platform isolation, apps-are-leaves, and API-crate purity
# (handled separately via API_CRATES) — are what M8 actually locks.
FORBIDDEN = {
    "tool": {"tool", "platform", "skill", "command", "app", "example"},
    "platform": {"tool", "skill", "command", "app", "example"},
    "skill": {"tool", "platform", "command", "app", "example"},
    "command": {"tool", "platform", "app", "example"},
}


def classify(rel_manifest):
    parts = rel_manifest.replace(os.sep, "/").split("/")
    if parts[0] == "crates":
        parts = parts[1:]
    if parts[0] == "apps":
        return "app"
    if parts[0] == "tools":
        return "tool" if len(parts) > 2 else "monolith"
    if parts[0] in {"skill", "skills"}:
        return "skill"
    if parts[0] == "commands":
        return "command"
    if parts[0] == "platforms":
        return "platform"
    if parts[0] == "examples":
        return "example"
    return "engine"


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else ""
    m = json.load(sys.stdin)
    root = m["workspace_root"]
    names = {p["name"] for p in m["packages"]}
    pkgs = {p["name"]: p for p in m["packages"]}
    classes = {
        n: (
            "contract"
            if n in SHARED_WIRE_CONTRACTS
            else classify(os.path.relpath(p["manifest_path"], root))
        )
        for n, p in pkgs.items()
    }

    def ws_deps(p):
        return sorted(
            {
                d["name"]
                for d in p["dependencies"]
                if d["name"] in names
                and d["name"] != p["name"]
                and d.get("kind") != "dev"
            }
        )

    if mode == "--list":
        for n in sorted(pkgs):
            print("%-24s %-10s %s" % (n, classes[n], " ".join(ws_deps(pkgs[n]))))
        return 0

    violations = []
    for contract, allowed in sorted(SHARED_WIRE_CONTRACTS.items()):
        if contract not in pkgs:
            continue
        for dep in ws_deps(pkgs[contract]):
            if dep not in allowed:
                violations.append(
                    "%s (contract) depends on %s — shared wire contracts may depend only on %s"
                    % (contract, dep, ", ".join(sorted(allowed)))
                )

    for n in sorted(pkgs):
        if n in EXEMPT:
            continue
        c = classes[n]
        for d in ws_deps(pkgs[n]):
            dc = classes[d]
            if n in HARNESS_COMPONENT_CRATES and d in {"tui", "tui-core"}:
                violations.append("%s depends on UI implementation %s" % (n, d))
                continue
            if d == "harness-runtime" and c not in LEAVES:
                violations.append("%s depends on the Harness composition root" % n)
                continue
            # json-projection is the dependency-neutral exact-UTF-16 JSON
            # carrier re-exported by core's public source-preserving types.
            # The leaf contracts and device primitives are the foundation
            # edges; domain crates remain forbidden here.
            if n == "core" and d not in CORE_FOUNDATION_DEPS:
                violations.append(
                    "core depends on %s — shared contracts must stay below domain and telemetry crates" % d
                )
                continue
            # API-crate purity: tool-api / command-api stay abstract; `skill`
            # contains the shared skill implementation and is classified above.
            optional_builtin_skill = (
                n == "command-api"
                and d == "skill"
                and "dep:skill" in pkgs[n].get("features", {}).get("builtin-handlers", [])
                and any(
                    dep["name"] == d and dep.get("optional", False)
                    for dep in pkgs[n]["dependencies"]
                )
            )
            if n in API_CRATES and not optional_builtin_skill and (
                d in API_CRATES or dc in {"tool", "skill", "command", "platform", "app", "example", "monolith"}
            ):
                violations.append(
                    "%s (api) depends on %s (%s) — API crates must stay impl-free" % (n, d, dc)
                )
                continue
            if c == "command" and dc == "command":
                violations.append("%s (command) depends on sibling command %s" % (n, d))
                continue
            if dc in LEAVES and c not in LEAVES:
                violations.append(
                    "%s (%s) depends on leaf %s (%s) — apps/examples are leaves"
                    % (n, c, d, dc)
                )
                continue
            if c in FORBIDDEN and dc in FORBIDDEN[c]:
                violations.append(
                    "%s (%s) depends on %s (%s) — forbidden by §8.1" % (n, c, d, dc)
                )

    for n in sorted(LEAF_LIBRARIES & set(pkgs)):
        outside = [d for d in ws_deps(pkgs[n]) if d not in LEAF_LIBRARIES]
        if outside:
            violations.append(
                "%s depends on %s — a leaf library may depend only on the other leaf libraries"
                % (n, ", ".join(outside))
            )

    for n in sorted(pkgs):
        if n in EXEMPT:
            continue
        for dep in pkgs[n]["dependencies"]:
            if dep["name"] in LOCAL_APP_CRATES:
                violations.append(
                    "%s depends on %s — the Local App implementation crates are gone; nothing may name one"
                    % (n, dep["name"])
                )

    for root_name in sorted(HARNESS_COMPONENT_CRATES):
        pending = [(root_name, [root_name])]
        seen = set()
        while pending:
            name, path = pending.pop()
            if name in seen:
                continue
            seen.add(name)
            if name in {"tui", "tui-core"}:
                violations.append("Harness reaches UI implementation: " + " -> ".join(path))
                continue
            pending.extend((dep, path + [dep]) for dep in ws_deps(pkgs[name]))

    if violations:
        sys.stderr.write("[deny] dependency-graph violations (%d):\n" % len(violations))
        for v in violations:
            sys.stderr.write("  - " + v + "\n")
        return 1

    print(
        "check-deps: OK — %d workspace crates, no §8.1 dependency violations"
        % len(pkgs)
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
