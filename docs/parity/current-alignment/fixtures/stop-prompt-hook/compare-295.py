#!/usr/bin/env python3
"""Summarize Native-only Stop prompt-hook raw request evidence for 293/294/295."""

import hashlib
import json
from pathlib import Path
import re


ROOT = Path(__file__).resolve().parent
BASELINE = ROOT / "native-baseline"
VERSIONS = ("2.1.293", "2.1.294", "2.1.295")


def sha256(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def strings(value: object) -> list[str]:
    if isinstance(value, str):
        return [value]
    if isinstance(value, dict):
        return [text for child in value.values() for text in strings(child)]
    if isinstance(value, list):
        return [text for child in value for text in strings(child)]
    return []


def raw_fields(version: str) -> dict[str, bytes]:
    request_dir = BASELINE / version / "requests"
    return {
        field: (request_dir / f"request-02.{field}.raw.json").read_bytes()
        for field in ("system", "messages")
    }


def field_record(version: str, field: str, raw: bytes) -> dict:
    return {
        "path": f"native-baseline/{version}/requests/request-02.{field}.raw.json",
        "bytes": len(raw),
        "sha256": sha256(raw),
    }


def main() -> int:
    loaded = {version: raw_fields(version) for version in VERSIONS}
    receipts = {
        version: json.loads((BASELINE / version / "native-receipt.json").read_text(encoding="utf-8"))
        for version in VERSIONS
    }
    result: dict = {
        "scope": "Native public Stop prompt-hook captures only; no candidate parity claim.",
        "versions": {
            version: {
                "native_version": receipts[version]["native_version"],
                "native_sha256": receipts[version]["native_sha256"],
                "hook_request_count": receipts[version]["hook_request_count"],
                "hook_marker_observed": receipts[version]["hook_marker_observed_in_original_request_bytes"],
                "native_exit_code": receipts[version]["native_exit_code"],
                "stdout_result": receipts[version]["stdout_result"],
            }
            for version in VERSIONS
        },
        "request_fields": {},
    }

    parsed_system = {version: json.loads(loaded[version]["system"]) for version in VERSIONS}
    parsed_messages = {version: json.loads(loaded[version]["messages"]) for version in VERSIONS}
    system_strings = {version: strings(parsed_system[version]) for version in VERSIONS}
    message_strings = {version: strings(parsed_messages[version]) for version in VERSIONS}

    for field in ("system", "messages"):
        result["request_fields"][field] = {
            "raw_bytes_equal_to_2.1.293": {
                version: loaded[version][field] == loaded["2.1.293"][field]
                for version in VERSIONS
            },
            "raw": {
                version: field_record(version, field, loaded[version][field])
                for version in VERSIONS
            },
        }

    billing = {}
    evaluator = {}
    for version in VERSIONS:
        all_system = "\n".join(system_strings[version])
        billing_match = re.search(r"x-anthropic-billing-header: ([^\n]+)", all_system)
        billing[version] = billing_match.group(1) if billing_match else None
        evaluator[version] = next(
            (value for value in system_strings[version] if "You are evaluating a stop-condition hook" in value),
            None,
        )
    result["system_content_observations"] = {
        "captured_billing_header_by_run": billing,
        "stop_evaluator_new_ok_policy_present": {
            version: bool(evaluator[version] and '"ok" decides what happens next' in evaluator[version])
            for version in VERSIONS
        },
        "2.1.294_and_2.1.295_evaluator_prompt_text_equal": evaluator["2.1.294"] == evaluator["2.1.295"],
        "classification": "The evaluator allow/block and untrusted-event-JSON instructions appear in 2.1.294 and remain unchanged in the 2.1.295 captured system field. The billing text is recorded as captured; its suffix differs between fixtures even for the same binary, so it is not treated as a fixed version-only delta.",
    }

    dynamic_ids = {}
    runtime_context = {}
    for version in VERSIONS:
        joined = "\n".join(message_strings[version])
        ids = re.findall(r'"prompt_id"\s*:\s*"([^"]+)"', joined)
        dynamic_ids[version] = [{
            "value": value,
            "uuid_v4_shape": bool(re.fullmatch(
                r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}", value
            )),
        } for value in ids]
        dates = re.findall(r"Today's date is (\d{4}-\d{2}-\d{2})", joined)
        runtime_context[version] = {
            "git_status_snapshot_present": "# gitStatus" in joined,
            "injected_session_dates": dates,
        }
    result["messages_dynamic_and_runtime_constraints"] = {
        "prompt_ids": dynamic_ids,
        "context_observations": runtime_context,
        "raw_fields_compared_without_normalization": True,
        "classification": "The hook prompt_id is a generated UUID reference. The messages field also contains a captured git-status snapshot and injected date, which changed between runs; therefore whole messages bytes remain unequal and those context changes must not be attributed to the binary version.",
    }

    output = BASELINE / "comparison-2.1.293-vs-2.1.294-vs-2.1.295.json"
    output.write_text(json.dumps(result, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result, ensure_ascii=True, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
