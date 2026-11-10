#!/usr/bin/env python3
"""Build conversation-body fixtures from retained, unmodified 2.1.319 requests.

This does not execute Claude Code or contact a provider. Run the separate live
capture script against an isolated loopback API to refresh the source requests.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re

VERSION = "2.1.319"
BINARY_SHA256 = "1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd"
MODELS = ["claude-sonnet-4-6", "claude-opus-5", "claude-sonnet-5-5"]
GROUPS = [("live-native-r3", False), ("live-native-memory", True)]
MEMORY_PATH = re.compile(r"You have a persistent(?:,)? file-based memory(?: system)? at `([^`]+)`")
BRANDING = [("Claude Code", "LingXi"), ("CLAUDE.md", "LINGXI.md")]
HOST_AVAILABILITY = {
    "upstream": " - LingXi is available as a CLI in the terminal, desktop app (Mac/Windows), web app (claude.ai/code), and IDE extensions (VS Code, JetBrains).",
    "host": " - LingXi runs in the current host client. Use the interfaces and capabilities explicitly exposed in this session.",
}


def expected_host_body(upstream):
    lines = upstream.split("\n")
    assert lines.count(HOST_AVAILABILITY["upstream"]) == 1, "Expected exactly one complete upstream availability line"
    return "\n".join(HOST_AVAILABILITY["host"] if line == HOST_AVAILABILITY["upstream"] else line for line in lines)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def build(root):
    cases = []
    for group, memory_enabled in GROUPS:
        report_bytes = (root / group / "report.json").read_bytes()
        report = json.loads(report_bytes)
        assert report["version"] == VERSION
        assert report["sha256"] == BINARY_SHA256
        for model in MODELS:
            path = root / group / model / "request-01.json"
            raw = path.read_bytes()
            request = json.loads(raw)
            assert request["model"] == model
            case_report = next(row for row in report["cases"] if row["model"] == model)
            assert case_report["passed"] and not case_report["refused_paths"]
            assert case_report.get("memory_enabled", False) == memory_enabled
            system = request["system"]
            assert len(system) == 3 and all(block["type"] == "text" for block in system)
            native = system[2]["text"]
            assert native.startswith("\nYou are an agent working with the user")
            expected = native
            for original, replacement in BRANDING:
                expected = expected.replace(original, replacement)
            memory_paths = MEMORY_PATH.findall(expected)
            if memory_enabled:
                assert len(memory_paths) == 1, (model, memory_paths)
                native_path = memory_paths[0]
                assert Path(native_path).is_absolute() and native_path.endswith("/memory/")
                assert expected.count(native_path) == 1
                expected = expected.replace(native_path, "<MEMORY_DIR>")
            else:
                assert not memory_paths
            cases.append({
                "name": model + ("-memory-on" if memory_enabled else "-memory-off"),
                "input": {
                    "model": model,
                    "mode": "code",
                    "is_interactive": False,
                    "skills_available": False,
                    "exclude_dynamic_sections": False,
                    "memory_dir": "<MEMORY_DIR>" if memory_enabled else None,
                    "tool_names": [tool["name"] for tool in request.get("tools", [])],
                },
                "capture": {
                    "path": str(path.relative_to(root)),
                    "sha256": digest(raw),
                    "report_sha256": digest(report_bytes),
                    "native_system_blocks": len(system),
                    "native_body_bytes": len(native.encode()),
                    "native_body_sha256": digest(native.encode()),
                },
                "expected_body": expected,
                "expected_body_sha256": digest(expected.encode()),
                "expected_host_body_sha256": digest(expected_host_body(expected).encode()),
            })
    return {
        "version": VERSION,
        "binary_sha256": BINARY_SHA256,
        "scope": "Default Code conversation body from an unmodified native executable; isolated configuration, fake credentials and a scripted loopback API; no real provider model call or model-quality claim.",
        "normalization": [
            "Omit the separate billing and branding system blocks; compare the complete third system block.",
            "Replace the literal product name Claude Code with LingXi and instruction filename CLAUDE.md with LINGXI.md.",
            "For memory-enabled cases only, replace the single native persistent-memory directory with <MEMORY_DIR>.",
            "At comparison time only, replace the one complete product-availability line using host_line_allowlist; retain expected_body and its upstream-derived digest unchanged.",
        ],
        "host_line_allowlist": [HOST_AVAILABILITY],
        "cases": cases,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("captures", type=Path, help="Directory containing live-native-r3 and live-native-memory")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    target = Path(__file__).resolve().parents[2] / "crates/orchestrator/tests/fixtures/prompt_session_2_1_319.json"
    output = json.dumps(build(args.captures.resolve(strict=True)), ensure_ascii=False, indent=2) + "\n"
    if args.check:
        assert target.read_text() == output, "Retained native conversation fixture differs"
        print("Verified six complete Claude Code 2.1.319 conversation-body fixtures")
    else:
        target.write_text(output)
        print("Wrote " + str(target))


if __name__ == "__main__":
    main()
