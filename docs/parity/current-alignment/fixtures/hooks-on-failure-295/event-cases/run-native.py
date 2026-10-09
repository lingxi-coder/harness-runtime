#!/usr/bin/env python3
"""Capture bounded Native 2.1.295 PreToolUse/Stop onFailure behavior."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import threading


ROOT = Path(__file__).resolve().parent
FIXTURE_ROOT = ROOT.parent
OUTPUT = FIXTURE_ROOT / "run-output"
FIXTURE_PATH = ROOT / "cases.json"
FIXTURE = json.loads(FIXTURE_PATH.read_text(encoding="utf-8"))
CASE_BY_ID = {item["id"]: item for item in FIXTURE["cases"]}
DEFAULT_BINARY = Path("/Users/luolingfeng/.local/share/claude/versions/2.1.295")
DUMMY_KEY = "native-hook-event-onfailure-295-dummy-key"
SHA = FIXTURE["native_sha256"]


def digest(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def make_sse(response_id: str, tool_use: dict | None, text: str) -> bytes:
    if tool_use is None:
        block = {"type": "text", "text": ""}
        delta = {"type": "text_delta", "text": text}
        stop_reason = "end_turn"
    else:
        block = {"type": "tool_use", "id": tool_use["id"], "name": "Bash", "input": {}}
        delta = {
            "type": "input_json_delta",
            "partial_json": json.dumps(tool_use["input"], ensure_ascii=True, separators=(",", ":")),
        }
        stop_reason = "tool_use"
    events = [
        ("message_start", {"type": "message_start", "message": {
            "id": response_id, "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [],
            "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 8, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": block}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": delta}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
                            "delta": {"stop_reason": stop_reason, "stop_sequence": None},
                            "usage": {"output_tokens": 4}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "".join(
        f"event: {name}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, value in events
    ).encode("ascii")


def hook_script_for(case: dict, output: Path) -> Path:
    script = output / "hook-failure.sh"
    if case["event"] == "PreToolUse":
        input_path = output / "pretool-hook-input.raw.json"
        invocation_path = output / "pretool-hook-invocation.marker"
        source = (
            "#!/bin/sh\n"
            f"cat > {shlex.quote(str(input_path))}\n"
            f"printf '%s\\n' 'ran' > {shlex.quote(str(invocation_path))}\n"
            "printf '%s\\n' 'FIXED_PRETOOL_HOOK_STDOUT'\n"
            "printf '%s\\n' 'FIXED_PRETOOL_HOOK_STDERR' >&2\n"
            "exit 1\n"
        )
    else:
        counter_path = output / "stop-hook-attempt-count"
        source = (
            "#!/bin/sh\n"
            "count=0\n"
            f"if [ -f {shlex.quote(str(counter_path))} ]; then count=$(cat {shlex.quote(str(counter_path))}); fi\n"
            "count=$((count + 1))\n"
            f"printf '%s\\n' \"$count\" > {shlex.quote(str(counter_path))}\n"
            f"cat > {shlex.quote(str(output / 'stop-hook-input-'))}$count.raw.json\n"
            f"printf '%s\\n' \"attempt-$count\" > {shlex.quote(str(output / 'stop-hook-attempt-'))}$count.marker\n"
            "printf '%s\\n' \"FIXED_STOP_HOOK_STDOUT_ATTEMPT_$count\"\n"
            "printf '%s\\n' \"FIXED_STOP_HOOK_STDERR_ATTEMPT_$count\" >&2\n"
            "if [ \"$count\" -eq 1 ]; then exit 1; fi\n"
            "exit 0\n"
        )
    script.write_text(source, encoding="utf-8")
    return script


def settings_for(case: dict, script: Path) -> dict:
    handler = {"type": "command", "command": "/bin/sh", "args": [str(script)], "timeout": 5}
    if case["on_failure"] is not None:
        handler["onFailure"] = case["on_failure"]
    event_entry = {"hooks": [handler]}
    if case["event"] == "PreToolUse":
        event_entry["matcher"] = "Bash"
    return {"hooks": {case["event"]: [event_entry]}}


def message_summary(raw_request: bytes) -> list[dict]:
    try:
        request = json.loads(raw_request)
    except (UnicodeDecodeError, json.JSONDecodeError):
        return []
    result = []
    for index, message in enumerate(request.get("messages", [])):
        if not isinstance(message, dict):
            continue
        content = message.get("content")
        observed = []
        if isinstance(content, str) and not content.startswith("<system-reminder>"):
            observed.append({"kind": "text", "text": content})
        elif isinstance(content, list):
            for block in content:
                if isinstance(block, dict) and block.get("type") in ("tool_result", "tool_use"):
                    observed.append({"kind": block.get("type"), "content": block})
                elif (
                    isinstance(block, dict)
                    and block.get("type") == "text"
                    and isinstance(block.get("text"), str)
                    and not block["text"].startswith("<system-reminder>")
                    and len(block["text"]) <= 1000
                ):
                    observed.append({"kind": "text", "text": block["text"]})
        result.append({"index": index, "role": message.get("role"), "content": observed})
    return result


def run_case(case: dict, binary: Path) -> dict:
    output = OUTPUT / case["id"]
    if output.exists():
        shutil.rmtree(output)
    output.mkdir(parents=True)
    dirs = {name: output / name for name in ("cwd", "home", "config", "tmp")}
    for path in dirs.values():
        path.mkdir()
    script = hook_script_for(case, output)
    settings = settings_for(case, script)
    settings_raw = json.dumps(settings, ensure_ascii=True, indent=2).encode("utf-8") + b"\n"
    settings_path = output / "settings.json"
    settings_path.write_bytes(settings_raw)

    pretool_input = {
        "command": f"printf 'PRETOOL_SIDE_EFFECT' > {shlex.quote(str(output / 'bash-side-effect.marker'))}; printf 'PRETOOL_STDOUT\\n'",
        "description": "fixed harmless marker write",
    }
    captured_requests: list[dict] = []
    captured_responses: list[dict] = []
    request_summaries = []

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def do_POST(self) -> None:
            raw = self.rfile.read(int(self.headers.get("content-length", "0")))
            accepted = (
                self.client_address[0] == "127.0.0.1"
                and self.path == "/v1/messages?beta=true"
                and self.headers.get("x-api-key") == DUMMY_KEY
            )
            sequence = len(captured_requests) + 1
            if case["event"] == "PreToolUse" and sequence == 1:
                tool_use = {"id": "toolu_native_pretool_failure_295", "input": pretool_input}
                response_text = ""
            else:
                tool_use = None
                if case["event"] == "Stop" and sequence == 1:
                    response_text = "FIXED_STOP_HOOK_FIRST_RESPONSE"
                elif case["event"] == "Stop":
                    response_text = "FIXED_STOP_HOOK_RETRY_RESPONSE"
                else:
                    response_text = FIXTURE["final_text"]
            body = make_sse(
                f"msg_native_hook_event_295_{case['id']}_{sequence}",
                tool_use,
                response_text,
            )
            route = "anthropic_messages" if accepted else "unexpected"
            headers = list(self.headers.raw_items())
            request_path = f"model-request-{sequence:02}.raw.json"
            response_path = f"model-response-{sequence:02}.raw.sse"
            (output / request_path).write_bytes(raw)
            (output / response_path).write_bytes(body)
            captured_requests.append({
                "sequence": sequence, "route": route, "method": "POST", "path": self.path,
                "accepted_dummy_key_loopback": accepted, "request_bytes": len(raw),
                "request_sha256": digest(raw), "request_path": request_path,
                "headers_observed_in_order": headers,
            })
            captured_responses.append({
                "sequence": sequence, "path": response_path, "bytes": len(body),
                "sha256": digest(body), "kind": "tool_use" if tool_use else "text",
                "tool_use_id": tool_use["id"] if tool_use else None,
                "text": response_text if not tool_use else None,
            })
            request_summaries.append({"sequence": sequence, "messages": message_summary(raw)})
            self.send_response(200 if accepted else 404)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            self.wfile.flush()

    server = HTTPServer(("127.0.0.1", 0), Handler)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    binary_version = subprocess.run([str(binary), "--version"], capture_output=True, timeout=15, check=False)
    version_text = (binary_version.stdout + binary_version.stderr).decode("utf-8", errors="backslashreplace").strip()
    if binary_version.returncode != 0 or version_text != "2.1.295 (Claude Code)" or digest(binary.read_bytes()) != SHA:
        server.shutdown()
        server.server_close()
        worker.join(timeout=2)
        raise SystemExit(f"pinned Native identity mismatch: {version_text!r} {digest(binary.read_bytes())}")

    argv = [
        str(binary), "--setting-sources", "project", "--settings", str(settings_path),
        "--strict-mcp-config", "--print", "--output-format", "json", "--tools",
        "Bash" if case["event"] == "PreToolUse" else "", "--max-turns", "3",
        "--max-budget-usd", "1", "--model", FIXTURE["model"], "--session-id", case["session_id"],
        "--debug", "hooks",
    ]
    if case["event"] == "PreToolUse":
        argv.extend(["--allowedTools", "Bash"])
    argv.extend(["--dangerously-skip-permissions", "--allow-dangerously-skip-permissions"])
    prompt = FIXTURE["pre_tool_prompt"] if case["event"] == "PreToolUse" else FIXTURE["stop_prompt"]
    argv.append(prompt)
    env = {
        "PATH": "/usr/bin:/bin", "HOME": str(dirs["home"]), "TMPDIR": str(dirs["tmp"]),
        "CLAUDE_CONFIG_DIR": str(dirs["config"]), "LANG": "C.UTF-8", "TZ": "UTC", "TERM": "dumb",
        "ANTHROPIC_API_KEY": DUMMY_KEY, "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "ANTHROPIC_MAX_RETRIES": "0", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1", "DO_NOT_TRACK": "1",
    }
    try:
        try:
            result = subprocess.run(argv, cwd=dirs["cwd"], env=env, capture_output=True,
                                    timeout=FIXTURE["timeout_seconds"], check=False)
            stdout, stderr, exit_code, timed_out = result.stdout, result.stderr, result.returncode, False
        except subprocess.TimeoutExpired as error:
            stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=2)

    (output / "native.stdout.raw.json").write_bytes(stdout)
    (output / "native.stderr.raw").write_bytes(stderr)
    try:
        parsed_stdout = json.loads(stdout)
        stdout_json_error = None
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        parsed_stdout = None
        stdout_json_error = type(error).__name__
    session_paths = sorted(dirs["config"].rglob(case["session_id"] + ".jsonl"))
    session_raw = session_paths[0].read_bytes() if len(session_paths) == 1 else b""
    if session_raw:
        (output / "native-session.raw.jsonl").write_bytes(session_raw)
    hook_inputs = []
    for path in sorted(output.glob("*-hook-input-*.raw.json")) + sorted(output.glob("*-hook-input.raw.json")):
        raw = path.read_bytes()
        hook_inputs.append({"path": path.name, "bytes": len(raw), "sha256": digest(raw)})
    bash_marker = output / "bash-side-effect.marker"
    invocation_markers = sorted(output.glob("*-hook-invocation.marker")) + sorted(output.glob("stop-hook-attempt-*.marker"))
    receipt = {
        "native_version": version_text, "native_sha256": digest(binary.read_bytes()),
        "native_path": str(binary), "case": case, "settings": settings,
        "hook_script": {"path": script.name, "bytes": script.stat().st_size,
                        "sha256": digest(script.read_bytes())},
        "settings_sha256": digest(settings_raw), "argv": argv,
        "public_entrypoint": "Native 2.1.295 --print --output-format json with isolated classic PreToolUse or Stop command hook and local Messages stub",
        "network_boundary": {"provider": "loopback only", "dummy_key_only": True,
                             "hook_process": "local /bin/sh", "isolated_home_config_cwd": True,
                             "nonessential_traffic_disabled": True},
        "timeout_seconds": FIXTURE["timeout_seconds"], "max_turns": 3,
        "native_exit_code": exit_code, "timed_out": timed_out,
        "stdout": {"path": "native.stdout.raw.json", "bytes": len(stdout), "sha256": digest(stdout)},
        "stdout_json_error": stdout_json_error,
        "stdout_summary": ({key: parsed_stdout.get(key) for key in (
            "type", "subtype", "is_error", "result", "session_id", "num_turns", "stop_reason"
        ) if key in parsed_stdout} if isinstance(parsed_stdout, dict) else None),
        "stderr": {"path": "native.stderr.raw", "bytes": len(stderr), "sha256": digest(stderr)},
        "http_requests": captured_requests, "http_responses": captured_responses,
        "request_message_summaries": request_summaries,
        "hook_inputs": hook_inputs,
        "hook_invocation_marker_count": len(invocation_markers),
        "hook_invocation_markers": [{"path": path.name, "bytes": path.stat().st_size,
                                     "sha256": digest(path.read_bytes())} for path in invocation_markers],
        "bash_side_effect": ({"path": bash_marker.name, "bytes": bash_marker.stat().st_size,
                              "sha256": digest(bash_marker.read_bytes()),
                              "content_ascii": bash_marker.read_bytes().decode("ascii", errors="backslashreplace")}
                             if bash_marker.exists() else None),
        "session_jsonl": ({"path": "native-session.raw.jsonl", "bytes": len(session_raw),
                           "sha256": digest(session_raw)} if session_raw else None),
        "scope": "Native classic hook failure policy for the named event and exit-1 command only; request/IDs/timestamps remain captured raw, not normalized.",
    }
    (output / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({
        "case": case["id"], "event": case["event"], "on_failure": case["on_failure"],
        "exit_code": exit_code, "timed_out": timed_out,
        "provider_requests": len(captured_requests), "hook_inputs": hook_inputs,
        "hook_invocations": len(invocation_markers),
        "bash_side_effect": receipt["bash_side_effect"], "stdout_summary": receipt["stdout_summary"],
        "session_jsonl": receipt["session_jsonl"], "receipt": str((output / "receipt.json").relative_to(FIXTURE_ROOT)),
    }, ensure_ascii=True, indent=2), flush=True)
    return 1 if timed_out or exit_code != 0 else 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--case", choices=sorted(CASE_BY_ID))
    parser.add_argument("--binary", default=os.environ.get("CLAUDE_NATIVE_BINARY", str(DEFAULT_BINARY)))
    args = parser.parse_args()
    binary = Path(args.binary).expanduser().resolve()
    if not binary.is_file():
        raise SystemExit(f"Native binary not found: {binary}")
    if digest(binary.read_bytes()) != SHA:
        raise SystemExit(f"Native 2.1.295 SHA mismatch: {digest(binary.read_bytes())}")
    selected = [CASE_BY_ID[args.case]] if args.case else FIXTURE["cases"]
    statuses = [run_case(case, binary) for case in selected]
    return 1 if any(statuses) else 0


if __name__ == "__main__":
    raise SystemExit(main())
