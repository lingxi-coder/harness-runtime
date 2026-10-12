#!/usr/bin/env python3
"""Negative probes for the relocated dependency boundary, using Cargo-shaped data."""
import json
from pathlib import Path
import subprocess
import sys
import unittest

ENGINE = Path(__file__).resolve().parents[1] / "checks/check_deps.py"

class DependencyRules(unittest.TestCase):
    def run_gate(self, paths, edges, arguments=()):
        # The production gate traverses these four required runtime roots.
        paths = {**{n: f"crates/{n}" for n in
                   ("llm-runtime", "harness-runtime", "tool-api", "permission")}, **paths}
        metadata = {"workspace_root": "/runtime", "packages": [
            {"name": name, "manifest_path": f"/runtime/{path}/Cargo.toml",
             "dependencies": [dep if isinstance(dep, dict) else {"name": dep}
                              for dep in edges.get(name, [])]}
            for name, path in paths.items()
        ]}
        return subprocess.run([sys.executable, str(ENGINE), *arguments], input=json.dumps(metadata),
                              text=True, capture_output=True)

    def test_tool_sibling_is_rejected_under_crates(self):
        result = self.run_gate({"tool-web": "crates/tools/web", "tool-file": "crates/tools/file"},
                               {"tool-web": ["tool-file"]})
        self.assertEqual(result.returncode, 1)
        self.assertIn("tool-web (tool) depends on tool-file (tool)", result.stderr)

    def test_component_cannot_depend_on_harness(self):
        result = self.run_gate({"core": "crates/core"}, {"core": ["harness-runtime"]})
        self.assertEqual(result.returncode, 1)
        self.assertIn("core depends on the Harness composition root", result.stderr)

    LEAVES = {name: f"crates/{name}" for name in ("device-api", "json-projection", "mcp-wire", "rooted-fs")}

    def test_core_may_name_the_leaf_libraries(self):
        result = self.run_gate({"core": "crates/core", **self.LEAVES},
                               {"core": ["mcp-wire", "rooted-fs", "device-api", "json-projection"]})
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_core_may_reexport_mcp_wire_but_not_depend_on_mcp_domain(self):
        wire = self.run_gate({"core": "crates/core"}, {"core": ["mcp-wire"]})
        self.assertEqual(wire.returncode, 0, wire.stderr)

        domain = self.run_gate({"core": "crates/core", "mcp": "crates/mcp"},
                               {"core": ["mcp"]})
        self.assertEqual(domain.returncode, 1)
        self.assertIn("core depends on mcp — shared contracts must stay below", domain.stderr)

    def test_mcp_wire_is_classified_as_a_leaf_contract(self):
        result = self.run_gate(
            {"mcp-wire": "crates/mcp-wire", "json-projection": "crates/json-projection"},
            {"mcp-wire": ["json-projection"]},
            arguments=("--list",),
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        line = next(line for line in result.stdout.splitlines() if line.startswith("mcp-wire"))
        self.assertEqual(line.split(), ["mcp-wire", "contract", "json-projection"])

    def test_mcp_wire_contract_rejects_domain_dependencies(self):
        allowed = self.run_gate(
            {"mcp-wire": "crates/mcp-wire", "json-projection": "crates/json-projection"},
            {"mcp-wire": ["json-projection"]},
        )
        self.assertEqual(allowed.returncode, 0, allowed.stderr)

        domain = self.run_gate(
            {"mcp-wire": "crates/mcp-wire", "json-projection": "crates/json-projection",
             "mcp": "crates/mcp"},
            {"mcp-wire": ["json-projection", "mcp"]},
        )
        self.assertEqual(domain.returncode, 1)
        self.assertIn(
            "mcp-wire (contract) depends on mcp — shared wire contracts may depend only on json-projection",
            domain.stderr,
        )

    def test_any_crate_may_name_a_leaf_library(self):
        for consumer, crate in (("permission", "rooted-fs"), ("jsonrpc", "json-projection"),
                                ("harness-runtime", "mcp-wire"), ("tool-mobile", "device-api")):
            with self.subTest(consumer=consumer, crate=crate):
                result = self.run_gate({consumer: f"crates/{consumer}", **self.LEAVES}, {consumer: [crate]})
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_leaf_library_names_only_the_other_leaf_libraries(self):
        ok = self.run_gate({**self.LEAVES}, {"mcp-wire": ["json-projection"]})
        self.assertEqual(ok.returncode, 0, ok.stderr)
        result = self.run_gate({"core": "crates/core", **self.LEAVES}, {"mcp-wire": ["json-projection", "core"]})
        self.assertEqual(result.returncode, 1)
        self.assertIn("mcp-wire depends on core — a leaf library may depend only on the other leaf libraries",
                      result.stderr)

    def test_nothing_may_name_the_crates_that_implemented_local_app(self):
        # The crates are not in `paths`: an old git revision would bring them in from outside the workspace,
        # where the member-based rules cannot see the edge. This rule matches by name.
        for consumer in ("core", "harness-runtime", "platform-android"):
            for crate in ("local-apps", "local-app-builder-service", "local-app-builder-plugin",
                          "local-app-builder-contracts"):
                with self.subTest(consumer=consumer, crate=crate):
                    result = self.run_gate({consumer: f"crates/{consumer}"}, {consumer: [crate]})
                    self.assertEqual(result.returncode, 1)
                    self.assertIn(
                        f"{consumer} depends on {crate} — the Local App implementation crates are gone",
                        result.stderr,
                    )

    def test_the_rule_by_name_holds_when_the_crates_are_not_workspace_members(self):
        # The project's crates are not in `paths`: the member-based rules cannot see these edges, this one can.
        result = self.run_gate({"core": "crates/core", "mcp": "crates/mcp"},
                               {"core": ["local-app-builder-service"], "mcp": ["mcp-wire"]})
        self.assertEqual(result.returncode, 1)
        self.assertIn(
            "core depends on local-app-builder-service — the Local App implementation crates are gone",
            result.stderr,
        )
        self.assertNotIn("mcp depends on", result.stderr)

    def test_shared_core_cannot_depend_on_telemetry(self):
        result = self.run_gate({"core": "crates/core", "telemetry": "crates/telemetry"},
                               {"core": ["telemetry"]})
        self.assertEqual(result.returncode, 1)
        self.assertIn("shared contracts must stay below domain and telemetry crates", result.stderr)

    def test_runtime_cannot_reach_tui_transitively(self):
        result = self.run_gate({"helper": "crates/helper", "tui": "crates/tui"},
                               {"harness-runtime": ["helper"], "helper": ["tui"]})
        self.assertEqual(result.returncode, 1)
        self.assertIn("harness-runtime -> helper -> tui", result.stderr)

    def test_tool_can_depend_on_abstract_api(self):
        result = self.run_gate({"tool-web": "crates/tools/web"}, {"tool-web": ["tool-api"]})
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_skill_is_classified_as_an_implementation_crate(self):
        result = self.run_gate({"skill": "crates/skill"}, {}, arguments=("--list",))
        self.assertEqual(result.returncode, 0, result.stderr)
        line = next(line for line in result.stdout.splitlines() if line.startswith("skill"))
        self.assertEqual(line.split()[:2], ["skill", "skill"])

if __name__ == "__main__":
    unittest.main()
