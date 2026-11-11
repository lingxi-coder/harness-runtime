#!/usr/bin/env python3
"""Extend the owned six-tool CLI fixture with exact malformed UTF-16 resume.

Only a new output directory and a dummy loopback provider are used. No provider
account, remote model, inherited credential or user session is touched. The
existing lifecycle fixture is imported unchanged and supplies process ownership,
configuration isolation, transport and capture logic.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from unittest.mock import patch

import filesystem_cli_322_live as lifecycle

BaseLoopback = lifecycle.FilesystemLoopback
BOM = 0xFEFF
INITIAL = [BOM, 0xD800, *map(ord, "seed\n")]
AFTER_EDIT = [BOM, 0xD801, *map(ord, "seed\n")]
AFTER_WRITE = [BOM, 0xD802, *map(ord, "tail\n")]
AFTER_RESUME = [BOM, 0xD803, *map(ord, "tail\n")]
UTF16_RESUME_ID = "fs322_utf16_resume_edit"


def encode_units(units):
    return b"".join(unit.to_bytes(2, "little") for unit in units)


def string_units(text):
    raw = text.encode("utf-16-le", errors="surrogatepass")
    return [int.from_bytes(raw[index:index + 2], "little") for index in range(0, len(raw), 2)]


def strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, list):
        for child in value:
            yield from strings(child)
    elif isinstance(value, dict):
        for child in value.values():
            yield from strings(child)


def observe(path, expected):
    raw = path.read_bytes() if path.is_file() else b""
    return {
        "path": str(path), "bytes_hex": raw.hex(), "sha256": hashlib.sha256(raw).hexdigest(),
        "utf16_units": [int.from_bytes(raw[index:index + 2], "little") for index in range(0, len(raw), 2)],
        "bom_preserved": raw.startswith(b"\xff\xfe"), "even_byte_count": len(raw) % 2 == 0,
        "expected_units": expected, "exact": raw == encode_units(expected),
    }


class Utf16FilesystemLoopback(BaseLoopback):
    def __init__(self, root, work):
        super().__init__(root, work)
        self.target = work / "malformed.utf16"
        self.target.write_bytes(encode_units(INITIAL))
        self.disk_stages = {"initial": observe(self.target, INITIAL)}
        self.read_contains_lone_d800 = False
        self.read_contains_replacement = False
        self.utf16_resume_without_read = None
        self.dispatched = []
        # .utf16 leaves the base *.txt Glob fixture unchanged. Read routes the
        # file by its UTF-16LE BOM, preserving the unpaired code unit.
        self.seed.extend([
            ("Read", {"file_path": str(self.target)}),
            ("Edit", {"file_path": str(self.target), "old_string": "\ud800", "new_string": "\ud801"}),
            ("Write", {"file_path": str(self.target), "content": "\ud802tail\n"}),
        ])

    def tool(self, identity, name, value):
        self.dispatched.append({"id": identity, "name": name, "input": value})
        if identity in {"fs322_seed_09", "fs322_seed_10", UTF16_RESUME_ID}:
            # Exact partial_json payload used by the unchanged base Handler.
            raw = json.dumps(value, ensure_ascii=True).encode("ascii")
            (self.root / f"scripted-input-{identity}.json").write_bytes(raw)
        # The base Handler uses json.dumps (ensure_ascii=True) for every SSE
        # frame and partial_json value, so lone surrogates stay JSON escapes.
        return BaseLoopback.tool(identity, name, value)

    def response(self, body):
        messages = body.get("messages", [])
        results = {block["tool_use_id"]: block for block in lifecycle.blocks(messages)
                   if block.get("type") == "tool_result" and block.get("tool_use_id", "").startswith("fs322_")}
        if "fs322_seed_08" in results and "read" not in self.disk_stages:
            self.disk_stages["read"] = observe(self.target, INITIAL)
            read_strings = list(strings(results["fs322_seed_08"].get("content")))
            self.read_contains_lone_d800 = any(0xD800 in string_units(value) for value in read_strings)
            self.read_contains_replacement = any(0xFFFD in string_units(value) for value in read_strings)
        if "fs322_seed_09" in results and "edit" not in self.disk_stages:
            self.disk_stages["edit"] = observe(self.target, AFTER_EDIT)
        if "fs322_seed_10" in results and "write" not in self.disk_stages:
            self.disk_stages["write"] = observe(self.target, AFTER_WRITE)
        if "FS_CLI_RESUME_322" in json.dumps(messages, ensure_ascii=True):
            direct = results.get(UTF16_RESUME_ID)
            if direct is None:
                # This must be the first requested tool after the second
                # process starts. There is no Read fallback for this assertion.
                return self.tool(UTF16_RESUME_ID, "Edit", {
                    "file_path": str(self.target), "old_string": "\ud802", "new_string": "\ud803",
                })
            if self.utf16_resume_without_read is None:
                self.utf16_resume_without_read = {
                    "succeeded": not bool(direct.get("is_error")), "result": direct.get("content"),
                    "disk": observe(self.target, AFTER_RESUME),
                }
                self.disk_stages["resume_edit"] = self.utf16_resume_without_read["disk"]
            if direct.get("is_error"):
                self.tool_errors.append(direct)
                return [{"type": "text", "text": "FS_CLI_TOOL_ERROR_322"}], "end_turn"
        return super().response(body)


def wire_receipts(api):
    """Record actual client request bytes and exact historical tool inputs."""
    requests = []
    inputs = {}
    for ordinal, request in enumerate(api.requests, 1):
        path = api.root / f"request-{ordinal:03}.json"
        raw = path.read_bytes()
        requests.append({
            "path": str(path), "sha256": hashlib.sha256(raw).hexdigest(), "byte_length": len(raw),
            "escaped_lone_units": [f"{unit:04x}" for unit in range(0xD800, 0xD804)
                                   if f"\\u{unit:04x}".encode() in raw.lower()],
        })
        for block in lifecycle.blocks(request["body"].get("messages", [])):
            identity = block.get("id")
            if block.get("type") != "tool_use" or identity not in {"fs322_seed_09", "fs322_seed_10", UTF16_RESUME_ID}:
                continue
            source = block.get("input", {})
            inputs[identity] = {name: string_units(value) for name, value in source.items()
                                if name in {"old_string", "new_string", "content"} and isinstance(value, str)}
    expected = {
        "fs322_seed_09": {"old_string": [0xD800], "new_string": [0xD801]},
        "fs322_seed_10": {"content": [0xD802, *map(ord, "tail\n")]},
        UTF16_RESUME_ID: {"old_string": [0xD802], "new_string": [0xD803]},
    }
    # Native322 retains exact code units inside tools/transcripts but projects
    # malformed source strings to U+FFFD in outgoing provider history. This was
    # measured against the pinned artifact, rather than inferred from JSON.
    expected_native_wire = {identity: {name: [0xFFFD if 0xD800 <= unit <= 0xDFFF else unit for unit in units]
                                      for name, units in fields.items()}
                            for identity, fields in expected.items()}
    scripted = {}
    for identity, fields in expected.items():
        path = api.root / f"scripted-input-{identity}.json"
        if not path.is_file():
            continue
        raw = path.read_bytes()
        parsed = json.loads(raw)
        actual = {name: string_units(parsed[name]) for name in fields}
        scripted[identity] = {"path": str(path), "sha256": hashlib.sha256(raw).hexdigest(),
                              "ascii_json": raw.decode("ascii"), "utf16_input_units": actual,
                              "exact": actual == fields}
    return {"requests": requests, "provider_scripted_inputs": scripted,
            "provider_scripted_inputs_exact": len(scripted) == len(expected) and all(row["exact"] for row in scripted.values()),
            "actual_history_input_units": inputs, "source_input_units": expected,
            "expected_native_wire_input_units": expected_native_wire,
            "projection": "Native322 outgoing history replaces each lone surrogate with U+FFFD; tool execution and disk retain original units.",
            "matches_native_wire_projection": inputs == expected_native_wire}


def transcript_receipts(root, expected_inputs):
    """Keep source rows distinct from sanitized api-request-blob diagnostics."""
    sources, sidecars, blobs, files = {}, {}, {}, []
    expected_sidecars = {
        "fs322_seed_08": {"content": INITIAL[1:]},
        "fs322_seed_09": {"oldString": [0xD800], "newString": [0xD801], "originalFile": INITIAL},
        "fs322_seed_10": {"content": AFTER_WRITE, "originalFile": AFTER_EDIT},
        UTF16_RESUME_ID: {"oldString": [0xD802], "newString": [0xD803], "originalFile": AFTER_WRITE},
    }
    for path in sorted((root / "config" / "projects").rglob("*.jsonl")):
        raw = path.read_bytes()
        files.append({"path": str(path), "sha256": hashlib.sha256(raw).hexdigest(),
                      "byte_length": len(raw), "escaped_lone_units": {
                          f"{unit:04x}": raw.lower().count(f"\\u{unit:04x}".encode())
                          for unit in range(0xD800, 0xD804)}})
        for line, row_raw in enumerate(raw.splitlines(), 1):
            if not row_raw:
                continue
            row = json.loads(row_raw)
            destination = sources if row.get("type") == "assistant" else blobs if row.get("type") == "api-request-blob" else None
            if destination is not None:
                for block in lifecycle.blocks(row.get("message", row)):
                    identity = block.get("id")
                    if block.get("type") == "tool_use" and identity in expected_inputs:
                        fields = block.get("input", {})
                        destination[identity] = {name: string_units(fields[name]) for name in expected_inputs[identity] if isinstance(fields.get(name), str)}
            if row.get("type") != "user":
                continue
            for block in lifecycle.blocks(row.get("message", {})):
                identity = block.get("tool_use_id")
                if block.get("type") != "tool_result" or identity not in expected_sidecars:
                    continue
                sidecar = row.get("toolUseResult", row.get("tool_use_result", {}))
                if identity == "fs322_seed_08" and isinstance(sidecar, dict):
                    sidecar = sidecar.get("file", {})
                sidecars[identity] = {name: string_units(sidecar[name]) for name in expected_sidecars[identity] if isinstance(sidecar, dict) and isinstance(sidecar.get(name), str)}
    return {"files": files, "assistant_input_units": sources, "expected_assistant_input_units": expected_inputs,
            "tool_use_result_units": sidecars, "expected_tool_use_result_units": expected_sidecars,
            "api_request_blob_input_units": blobs,
            "source_rows_exact": sources == expected_inputs, "tool_use_result_exact": sidecars == expected_sidecars}


def run(binary, engine, root, timeout, setup_fake_account=False):
    created = []

    def make_api(provider_root, work):
        api = Utf16FilesystemLoopback(provider_root, work)
        created.append(api)
        return api

    # Inject only the API constructor in this process; the imported source and
    # base six-tool runner remain byte-for-byte unchanged on disk.
    with patch.object(lifecycle, "FilesystemLoopback", make_api):
        report = lifecycle.run(binary, engine, root, timeout, setup_fake_account)
    assert len(created) == 1
    api = created[0]
    wire = wire_receipts(api)
    transcripts = transcript_receipts(root, wire["source_input_units"])
    resumed_tools = [item for item in api.dispatched if item["id"].startswith("fs322_resume_") or item["id"] == UTF16_RESUME_ID]
    first_resumed_is_direct_utf16_edit = bool(resumed_tools) and resumed_tools[0]["id"] == UTF16_RESUME_ID
    report["base_lifecycle_passed"] = report["passed"]
    report["utf16"] = {
        "disk_stages": api.disk_stages, "read_model_content_contains_lone_d800": api.read_contains_lone_d800,
        "read_model_content_contains_native_replacement": api.read_contains_replacement,
        "resume_without_read": api.utf16_resume_without_read,
        "first_resumed_tool_is_utf16_edit": first_resumed_is_direct_utf16_edit,
        "wire": wire, "transcripts": transcripts,
    }
    report["passed"] = bool(
        report["base_lifecycle_passed"] and first_resumed_is_direct_utf16_edit
        and api.utf16_resume_without_read and api.utf16_resume_without_read["succeeded"]
        and set(api.disk_stages) == {"initial", "read", "edit", "write", "resume_edit"}
        and all(stage["exact"] and stage["bom_preserved"] for stage in api.disk_stages.values())
        and api.read_contains_replacement and not api.read_contains_lone_d800
        and wire["matches_native_wire_projection"] and wire["provider_scripted_inputs_exact"]
        and transcripts["source_rows_exact"] and transcripts["tool_use_result_exact"]
        and report.get("resume_without_read", {}).get("succeeded")
    )
    report["fixture_sources"] = {
        str(Path(__file__).resolve()): hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        str(Path(lifecycle.__file__).resolve()): hashlib.sha256(Path(lifecycle.__file__).read_bytes()).hexdigest(),
    }
    report["meaning"] = "Real six-tool CLI lifecycle plus exact malformed UTF-16LE BOM/units across Read, Edit, Write and direct resumed Edit; no Read fallback for the malformed resumed edit."
    lifecycle.dump(root / "report.json", report)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--engine", choices=("native", "lingxi"), required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--setup-fake-account", action="store_true")
    parser.add_argument("--timeout", type=int, default=45)
    args = parser.parse_args()
    if not 1 <= args.timeout <= 120:
        parser.error("--timeout must be 1..120 seconds")
    args.output.mkdir(parents=True, exist_ok=False)
    report = run(args.binary, args.engine, args.output.resolve(), args.timeout, args.setup_fake_account)
    print(json.dumps({"engine": report["engine"], "passed": report["passed"],
                      "base_lifecycle_passed": report["base_lifecycle_passed"],
                      "utf16_resume_succeeded": bool(report["utf16"]["resume_without_read"] and report["utf16"]["resume_without_read"]["succeeded"]),
                      "matches_native_wire_projection": report["utf16"]["wire"]["matches_native_wire_projection"],
                      "scripted_input_units_exact": report["utf16"]["wire"]["provider_scripted_inputs_exact"],
                      "transcript_units_exact": report["utf16"]["transcripts"]["source_rows_exact"] and report["utf16"]["transcripts"]["tool_use_result_exact"],
                      "report": str(args.output / "report.json")}, ensure_ascii=True))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
