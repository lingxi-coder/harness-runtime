#!/usr/bin/env python3
"""Capture a real native or LingXi Read count request with a local dummy provider.

No external provider or credential is used. The listener never forwards a
request; every CLI, provider capture and file lives in a fresh owned directory.
"""
from __future__ import annotations

from filesystem_cli_322_live import credential_fixture

import argparse
import hashlib
import json
from pathlib import Path
import uuid

from filesystem_cli_322_live import (
    FilesystemLoopback, MODEL, NATIVE_SHA, blocks, dump, run_process,
)


def utf16le_units(value: str) -> bytes:
    """Hash/compare source code units without repairing unpaired surrogates."""
    return value.encode("utf-16-le", errors="surrogatepass")


def result_texts(result: dict) -> list[str]:
    content = result.get("content")
    if isinstance(content, str):
        return [content]
    if isinstance(content, list):
        return [block["text"] for block in content if isinstance(block, dict)
                and block.get("type") == "text" and isinstance(block.get("text"), str)]
    return []


def run(binary: Path, root: Path, timeout: int, engine: str = "native", utf16: bool = False, setup_fake_account=False):
    if engine not in ("native", "lingxi"):
        raise ValueError("engine must be native or lingxi")
    binary = binary.resolve(strict=True)
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    if engine == "native" and digest != NATIVE_SHA:
        raise ValueError("Native executable must match pinned Claude Code 2.1.322")
    for part in ("config", "tmp", "work", "captures"):
        (root / part).mkdir(parents=True)
    config = ".claude.json" if engine == "native" else ".lingxi.json"
    (root / "config" / config).write_text(json.dumps({"hasCompletedOnboarding": True}))
    if utf16:
        # Match the native120001-unit count capture exactly, excluding its BOM.
        source = "x" * 120_000 + "\ud800"
        disk_bytes = b"\xff\xfe" + utf16le_units(source)
        generation_source = "x" * 120_000 + "\ufffd"
    else:
        prefix, suffix = "READ_COUNT_SOURCE_322_", "_END_READ_322_"
        source = prefix + "x" * (120_000 - len(prefix) - len(suffix)) + suffix
        assert len(source) == 120_000
        disk_bytes = source.encode("utf-8")
        generation_source = source
    source_units = utf16le_units(source)
    # Preserve the original UTF-8 fixture hash while giving the malformed input
    # an explicit code-unit hash encoding rather than illegal/repaired UTF-8.
    source_hash_bytes = source_units if utf16 else disk_bytes
    source_hash_encoding = "utf-16-le-code-units-without-bom" if utf16 else "utf-8"
    target = root / "work/source.txt"
    target.write_bytes(disk_bytes)
    with FilesystemLoopback(root / "provider", root / "work") as api, credential_fixture(
        binary, engine, root, api, timeout, setup_fake_account
    ) as credentials:
        # The stub returns2500tokens; the rough30k estimate would truncate this
        # ordinary Read. The count request must carry the original source units.
        api.seed = [("Read", {"file_path": str(target)})]
        outcome, requests = run_process(
            binary, engine, root, api, "seed",
            ["--session-id", str(uuid.uuid4()), "--max-turns", "4"],
            "FS_CLI_SEED_322", "Read", timeout,
        )
        counts = [(index, row) for index, row in enumerate(api.requests, 1)
                  if "count_tokens" in row["path"]]
        count_rows = []
        for ordinal, row in counts:
            path = root / "provider" / f"request-{ordinal:03}.json"
            raw = path.read_bytes()
            body = row["body"]
            messages = body.get("messages", [])
            scalar = (isinstance(messages, list) and len(messages) == 1
                      and isinstance(messages[0], dict)
                      and set(messages[0]) == {"role", "content"}
                      and messages[0]["role"] == "user"
                      and isinstance(messages[0]["content"], str))
            content = messages[0]["content"] if scalar else ""
            content_units = utf16le_units(content)
            exact_source = scalar and content_units == source_units
            count_rows.append({
                "path": row["path"], "request_file": str(path),
                "request_sha256": hashlib.sha256(raw).hexdigest(),
                "request_bytes": len(raw), "top_level_keys": list(body),
                "only_count_fields": set(body) == {"model", "messages", "tools"},
                "model": body.get("model"), "model_matches": body.get("model") == MODEL,
                "single_scalar_source": exact_source,
                "source_sha256": hashlib.sha256(source_hash_bytes).hexdigest(),
                "source_hash_encoding": source_hash_encoding,
                "content_utf16_units": len(content_units) // 2,
                "content_utf16le_sha256": hashlib.sha256(content_units).hexdigest(),
                "content_units_tail": [int.from_bytes(content_units[index:index + 2], "little")
                                       for index in range(max(0, len(content_units) - 24), len(content_units), 2)],
                "source_bom_stripped": scalar and not content.startswith("\ufeff"),
                "lone_surrogate_codepoints": sum(0xD800 <= ord(char) <= 0xDFFF for char in content),
                "replacement_codepoints": content.count("\ufffd"),
                "tools_absent": "tools" not in body,
                "tools_empty": body.get("tools") == [],
                "system_absent": "system" not in body,
                "thinking_absent": "thinking" not in body,
                "output_config_absent": "output_config" not in body,
            })
        tool_results = [block for row in requests for block in blocks(row["body"].get("messages", []))
                        if block.get("type") == "tool_result" and block.get("tool_use_id") == "fs322_seed_00"]
        texts = [text for result in tool_results for text in result_texts(result)]
        complete_result = any(generation_source in text for text in texts)
        # Claude generation sanitizes unpaired units toFFFD after counting the
        # raw source. Do not mistake a raw count payload for the model result.
        generation_sanitized = bool(texts) and not any(
            0xD800 <= ord(char) <= 0xDFFF for text in texts for char in text)
        disk_unchanged = target.read_bytes() == disk_bytes
        strict_count = len(count_rows) == 1 and all(
            row["only_count_fields"] and row["model_matches"] and row["single_scalar_source"]
            and row["source_bom_stripped"] and row["tools_empty"] and row["system_absent"]
            and row["thinking_absent"] and row["output_config_absent"] for row in count_rows)
        # StartupCatalog performs one credential-free catalog refresh. The local
        # proxy still denies it; classify only this exact known authority apart
        # from unexpected provider/network routes. Native fixtures allow none.
        expected_refusals = [route for route in api.refused if engine == "lingxi"
                             and route == "CONNECT us-central1-lingxi-5e4b2.cloudfunctions.net:443"]
        unexpected_refusals = [route for route in api.refused if route not in expected_refusals]
        report = {
            "engine": engine, "binary": str(binary), "binary_sha256": digest,
            # A successful runtime probe is not proof of a source build. Root
            # attaches an independently verified build receipt when available.
            "source_build_verified": False,
            "native_reference_version": "2.1.322", "native_reference_sha256": NATIVE_SHA,
            "external_provider": False, "account_registration": False,
            "utf16": utf16, "source_bytes": len(disk_bytes),
            "source_utf16_units": len(source_units) // 2,
            "source_utf16le_sha256": hashlib.sha256(source_units).hexdigest(),
            "source_disk_sha256": hashlib.sha256(disk_bytes).hexdigest(),
            "generation_source_utf8_sha256": hashlib.sha256(generation_source.encode("utf-8")).hexdigest(),
            "local_estimate": (len(source_units) // 2 + 2) // 4,
            "provider_stub_tokens": 2_500, "outcome": outcome,
            "count_requests": count_rows, "strict_count_request": strict_count,
            "complete_tool_result": complete_result, "generation_source_sanitized": generation_sanitized,
            "disk_unchanged": disk_unchanged,
            "refused_routes": api.refused, "expected_startup_metadata_refusals": expected_refusals,
            "unexpected_refused_routes": unexpected_refusals, "tool_errors": api.tool_errors,
            "passed": outcome["completed"] and outcome["exit_code"] == 0
            and not outcome["timed_out"] and strict_count and complete_result
            and generation_sanitized and disk_unchanged and not api.tool_errors and not unexpected_refusals,
        }
        if engine == "native":
            # Retain original native report fields and its mandatory SHA guard.
            report.update(native_version="2.1.322", native_sha256=digest)
    report.update(credential_fixture=credentials, account_registration=credentials["local_named_account_created"],
                  external_account_registration=False)
    dump(root / "report.json", report)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--engine", choices=("native", "lingxi"), default="native")
    parser.add_argument("--setup-fake-account", action="store_true")
    parser.add_argument("--utf16", action="store_true", help="seed a BOM,120000 x units and a lone D800")
    parser.add_argument("--timeout", type=int, default=45)
    args = parser.parse_args()
    if not 1 <= args.timeout <= 120:
        parser.error("--timeout must be 1..120 seconds")
    args.output.mkdir(parents=True, exist_ok=False)
    result = run(args.binary, args.output.resolve(), args.timeout, engine=args.engine, utf16=args.utf16, setup_fake_account=args.setup_fake_account)
    print(json.dumps({"engine": result["engine"], "utf16": result["utf16"],
                      "passed": result["passed"], "count_requests": len(result["count_requests"]),
                      "complete_tool_result": result["complete_tool_result"],
                      "report": str(args.output / "report.json")}))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
