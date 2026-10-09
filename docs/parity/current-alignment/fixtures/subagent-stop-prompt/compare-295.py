#!/usr/bin/env python3
"""Summarize Native-only SubagentStop prompt-hook raw evidence for 293/294/295."""

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


def capture(version: str) -> tuple[dict, dict[str, bytes]]:
    version_dir = BASELINE / version
    receipt = json.loads((version_dir / "native-receipt.json").read_text(encoding="utf-8"))
    hook = next(item for item in receipt["requests"] if item.get("classification") == "subagent_stop_hook")
    fields = {
        field: (version_dir / "requests" / hook[f"{field}_artifact"]).read_bytes()
        for field in ("system", "messages")
    }
    return {"receipt": receipt, "hook": hook}, fields


def field_record(version: str, field: str, hook: dict, raw: bytes) -> dict:
    return {
        "path": f"native-baseline/{version}/requests/{hook[field + '_artifact']}",
        "bytes": len(raw),
        "sha256": sha256(raw),
    }


def main() -> int:
    loaded = {version: capture(version) for version in VERSIONS}
    result: dict = {
        "scope": "Native public Agent -> SubagentStop prompt-hook captures only; no candidate parity claim.",
        "versions": {},
        "hook_request_fields": {},
    }
    parsed = {}
    joined_messages = {}
    joined_system = {}
    for version in VERSIONS:
        details, fields = loaded[version]
        receipt, hook = details["receipt"], details["hook"]
        parsed[version] = {field: json.loads(raw) for field, raw in fields.items()}
        joined_messages[version] = "\n".join(strings(parsed[version]["messages"]))
        joined_system[version] = "\n".join(strings(parsed[version]["system"]))
        result["versions"][version] = {
            "native_version": receipt["native_version"],
            "native_sha256": receipt["native_sha256"],
            "classification_counts": receipt["classification_counts"],
            "hook_request_sequence": hook["sequence"],
            "event_name_observed": "\"hook_event_name\":\"SubagentStop\"" in joined_messages[version],
            "subagent_context_marker_observed": "cc_is_subagent=true" in joined_system[version],
            "child_request_observed": receipt["child_request_observed"],
            "parent_completed_with_fixed_response": receipt["parent_completed_with_fixed_response"],
            "native_exit_code": receipt["native_exit_code"],
            "stdout_result": receipt["stdout_result"],
        }

    for field in ("system", "messages"):
        result["hook_request_fields"][field] = {
            "raw_bytes_equal_to_2.1.293": {
                version: loaded[version][1][field] == loaded["2.1.293"][1][field]
                for version in VERSIONS
            },
            "raw": {
                version: field_record(version, field, loaded[version][0]["hook"], loaded[version][1][field])
                for version in VERSIONS
            },
        }

    billing = {}
    evaluator = {}
    for version in VERSIONS:
        billing_match = re.search(r"x-anthropic-billing-header: ([^\n]+)", joined_system[version])
        billing[version] = billing_match.group(1) if billing_match else None
        evaluator[version] = next(
            (text for text in strings(parsed[version]["system"]) if "You are evaluating a stop-condition hook" in text),
            None,
        )
    result["system_content_observations"] = {
        "captured_billing_header_by_run": billing,
        "stop_evaluator_new_ok_policy_present": {
            version: bool(evaluator[version] and '"ok" decides what happens next' in evaluator[version])
            for version in VERSIONS
        },
        "2.1.294_and_2.1.295_evaluator_prompt_text_equal": evaluator["2.1.294"] == evaluator["2.1.295"],
        "classification": "The allow/block and untrusted-event-JSON evaluator instructions are present in 2.1.294 and unchanged in the 2.1.295 captured system field. Billing text is recorded as captured; its suffix differs between fixtures even for the same binary, so it is not treated as a fixed version-only delta.",
    }

    dynamic_refs = {}
    runtime_context = {}
    for version in VERSIONS:
        text = joined_messages[version]
        prompt_ids = re.findall(r'"prompt_id"\s*:\s*"([^"]+)"', text)
        agent_ids = re.findall(r'"agent_id"\s*:\s*"([^"]+)"', text)
        transcript_paths = re.findall(r'"agent_transcript_path"\s*:\s*"([^"]+)"', text)
        dynamic_refs[version] = {
            "prompt_ids": [{
                "value": value,
                "uuid_v4_shape": bool(re.fullmatch(
                    r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}", value
                )),
            } for value in prompt_ids],
            "agent_ids": agent_ids,
            "agent_transcript_paths": transcript_paths,
        }
        dates = re.findall(r"Today's date is (\d{4}-\d{2}-\d{2})", text)
        runtime_context[version] = {
            "git_status_snapshot_present": "# gitStatus" in text,
            "injected_session_dates": dates,
        }
    result["messages_dynamic_and_runtime_constraints"] = {
        "generated_references": dynamic_refs,
        "context_observations": runtime_context,
        "raw_fields_compared_without_normalization": True,
        "classification": "The hook request includes generated prompt_id and agent_id references plus an agent transcript path containing the generated agent id. The messages field also carries a git-status snapshot and injected date that changed between runs; whole messages bytes remain unequal and these values are not normalized or attributed to the binary version.",
    }

    output = BASELINE / "comparison-2.1.293-vs-2.1.294-vs-2.1.295.json"
    output.write_text(json.dumps(result, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result, ensure_ascii=True, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
