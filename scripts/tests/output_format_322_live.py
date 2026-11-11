#!/usr/bin/env python3
"""Validate real account traffic against the captured shared 2.1.322 contract.

Real provider payloads, token counts and prices differ. This check preserves
their wire bytes and validates the common envelope, ledger boundaries, accepted
row identity, tool ordering and exact-account routing instead of equalizing
provider-specific values.
"""
import argparse
import hashlib
import json
import math
from pathlib import Path
from uuid import UUID

import output_format_322_shared as native


def ordered_schema(actual, reference, context, optional=frozenset()):
    required = [key for key in reference if key not in optional]
    assert all(key in actual for key in required), (context, "missing shared key", required, list(actual))
    assert [key for key in actual if key in reference] == [key for key in reference if key in actual], (context, "shared key order")


def verify(root, config_path, reference_root):
    config = json.loads(config_path.read_text())
    outcomes = json.loads((root / "outcomes.json").read_text())
    assert outcomes["exit_code"] == 0
    assert len(outcomes["cases"]) == len(config["cases"])
    raw = (root / "stdout.jsonl").read_bytes()
    assert raw.endswith(b"\n") and b"\r\n" not in raw
    lines = raw.decode("utf-8").splitlines()
    rows = [json.loads(line) for line in lines]
    reference = native.captures(reference_root / "ledger", "sdk-multi-turn", "native-a")[0]
    result_template = next(row for _, row in reference if row.get("type") == "result")
    assistant_template = next(row for _, row in reference if row.get("type") == "assistant")
    model_template = next(iter(result_template["modelUsage"].values()))
    snapshot = next((root / "account-state/provider-accounts").glob("acct_*.json"))
    metadata = {row["accountId"]: row for row in json.loads(snapshot.read_text())["accounts"]}
    stored = {}
    for path in (root / "session-state").rglob("*.jsonl"):
        for line in path.read_text().splitlines():
            row = json.loads(line)
            if row.get("type") in ("assistant", "user") and row.get("uuid"):
                stored[row["uuid"]] = row
    previous = {}
    pending_tool_ids = {}
    tool_result_batches = set()
    routing = []
    tool_results = 0
    accepted_frames = 0
    results = []
    for wire, row in zip(lines, rows):
        kind = row.get("type")
        if kind == "system" and row.get("subtype") == "provider_account_routing":
            routing.append(row["routing"])
        if kind in ("assistant", "user"):
            assert str(UUID(row["uuid"])) == row["uuid"]
            accepted = stored[row["uuid"]]
            assert row["timestamp"] == accepted["timestamp"]
            assert row["session_id"] == accepted["sessionId"]
            assert row["message"]["content"] == accepted["message"]["content"]
            accepted_frames += 1
            if kind == "assistant":
                ordered_schema(row, assistant_template, "assistant", {"request_id"})
                for block in row["message"]["content"]:
                    if block.get("type") == "tool_use":
                        assert block["id"] not in pending_tool_ids
                        pending_tool_ids[block["id"]] = (row["message"]["id"], row["message"]["model"])
            else:
                for block in row["message"].get("content", []):
                    if isinstance(block, dict) and block.get("type") == "tool_result":
                        assert block["tool_use_id"] in pending_tool_ids, "tool result precedes its assistant frame"
                        tool_result_batches.add(pending_tool_ids.pop(block["tool_use_id"]))
                        assert block.get("is_error") is False
                        assert block.get("content") == "LXI_LIVE_322_TOOL"
                        facts = [fact for fact in row.get("tool_result_meta", [])
                                 if fact.get("id") == block["tool_use_id"]]
                        assert len(facts) == 1, "accepted tool lost its permission facts"
                        assert facts[0]["permission_decision"] == {
                            "decision": "accept", "source": "user_permanent", "reason_type": "rule"}
                        tool_results += 1
        if kind != "result":
            continue
        index = len(results)
        case = config["cases"][index]
        assert row["subtype"] == "success" and row["is_error"] is False
        assert not pending_tool_ids
        assert row["num_turns"] == 1 + len(tool_result_batches), (case["label"], "query-user batch count")
        assert "LXI_LIVE_322_OK" in row["result"]
        assert row["result_index"] == index
        ordered_schema(row, result_template, (case["label"], "result"))
        keys = list(row)
        assert keys[keys.index("result_index") + 1] == "model_usage_since_last_result"
        ordered_schema(row["usage"], result_template["usage"], (case["label"], "usage"))
        selected = [event for event in routing if event.get("status") == "selected"]
        assert selected, (case["label"], "no accepted account routing")
        assert all(event["selectedAccountId"] == case["account_id"] for event in selected), (case["label"], "account changed")
        intervals = row["model_usage_since_last_result"]
        assert intervals, (case["label"], "empty interval")
        assert sum(usage["inputTokens"] + usage["cacheReadInputTokens"] for usage in intervals.values()) > 0
        assert sum(usage["outputTokens"] for usage in intervals.values()) > 0
        for model, current in row["modelUsage"].items():
            ordered_schema(current, model_template, (case["label"], model))
            delta = intervals.get(model, {})
            before = previous.get(model, {})
            for key in ["inputTokens", "outputTokens", "cacheReadInputTokens", "cacheCreationInputTokens", "webSearchRequests", "thinkingTokens", "cacheCreationEphemeral5mInputTokens", "cacheCreationEphemeral1hInputTokens"]:
                assert current[key] == before.get(key, 0) + delta.get(key, 0), (case["label"], model, key)
            assert math.isclose(current["costUSD"], before.get("costUSD", 0) + delta.get("costUSD", 0), rel_tol=1e-14, abs_tol=1e-15)
        assert math.isclose(row["total_cost_usd"], sum(usage["costUSD"] for usage in row["modelUsage"].values()), rel_tol=1e-14, abs_tol=1e-15)
        previous = row["modelUsage"]
        results.append({"label":case["label"], "account_id":case["account_id"],
            "provider":metadata[case["account_id"]]["providerId"], "model":case["model"],
            "result_index":index, "selected_requests":len(selected),
            "usage_raw_sha256":hashlib.sha256(native.field(wire, "usage")).hexdigest(),
            "model_usage_raw_sha256":hashlib.sha256(native.field(wire, "modelUsage")).hexdigest(),
            "interval_raw_sha256":hashlib.sha256(native.field(wire, "model_usage_since_last_result")).hexdigest(),
            "interval":intervals, "total_cost_usd":row["total_cost_usd"]})
        routing = []
        tool_result_batches = set()
    assert len(results) == len(config["cases"])
    assert len({row["provider"] for row in results}) >= 2
    assert len({row["account_id"] for row in results}) >= 3
    assert tool_results >= sum(bool(case.get("tool_probe")) for case in config["cases"])
    # Bind public accounting to the durable integer ledger and its actual
    # credential attribution, independently of the public interval accumulator.
    states = []
    for path in (root / "session-state/session-state").glob("*/ledger.v1.jsonl"):
        for line in path.read_text().splitlines():
            cost = json.loads(line).get("event", {}).get("Cost")
            if cost:
                states.append(cost["state"])
    assert states, "missing durable accounting"
    state = states[-1]
    accounts = state["per_account_usage"]
    assert {entry["attribution"]["account_id"] for entry in accounts} == {case["account_id"] for case in config["cases"]}
    assert sum(entry["cost_nano_usd"] for entry in accounts) == state["total_nano_usd"]
    assert abs(results[-1]["total_cost_usd"] * 1_000_000_000 - state["total_nano_usd"]) <= 1
    account_evidence = []
    for entry in accounts:
        attribution = entry["attribution"]
        account = attribution["account_id"]
        assert attribution["provider_id"] == metadata[account]["providerId"]
        assert entry["request_count"] == sum(result["selected_requests"] for result in results if result["account_id"] == account)
        account_evidence.append({"provider":attribution["provider_id"],"account_id":account,
            "model":entry["model"],"request_count":entry["request_count"],"cost_nano_usd":entry["cost_nano_usd"],
            "identity_generation":attribution["identity_generation"]})
    return {"passed":True,"native_version":"2.1.322","scope":__doc__,
        "real_network":True,"provider_count":len({r["provider"] for r in results}),
        "account_count":len({r["account_id"] for r in results}),"result_count":len(results),
        "accepted_frames":accepted_frames,"successful_tool_results":tool_results,"results":results,
        "durable_account_ledger":account_evidence,"total_nano_usd":state["total_nano_usd"],
        "provider_partial_frames":sum(row.get("type") == "stream_event" for row in rows),
        "partial_scope":"Provider-native partial forwarding is covered by the controlled Anthropic fixtures; these OpenAI-compatible live streams use canonical accepted assistant blocks.",
        "lingxi_extensions_retained":True,
        "provider_values_byte_equal_to_native":False,
        "stdout_sha256":hashlib.sha256(raw).hexdigest()}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--reference", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.write_text(json.dumps(verify(args.root, args.config, args.reference), indent=2) + "\n")
