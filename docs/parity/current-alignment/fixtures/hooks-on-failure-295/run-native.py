#!/usr/bin/env python3
"""Run one isolated Native 2.1.295 classic-hook onFailure public CLI probe."""

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
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
CASE_BY_ID = {case["id"]: case for case in FIXTURE["cases"]}
DEFAULT_BINARY = Path("/Users/luolingfeng/.local/share/claude/versions/2.1.295")


def sha256(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def make_sse(response_id: str, model: str, text: str) -> bytes:
    events = [
        ("message_start", {"type": "message_start", "message": {
            "id": response_id, "type": "message", "role": "assistant", "model": model,
            "content": [], "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 8, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                 "content_block": {"type": "text", "text": ""}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                 "delta": {"type": "text_delta", "text": text}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
                           "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                           "usage": {"output_tokens": 4}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "".join(
        f"event: {name}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, value in events
    ).encode("ascii")


def settings_for(case: dict, output: Path, port: int) -> dict:
    common_hook = {"type": "command"}
    if case["family"] == "command-spawn":
        missing = output / "does-not-exist-hook-command"
        common_hook.update({"command": str(missing), "args": [], "timeout": 5})
    elif case["family"] == "command-exit-1":
        script = output / "failing-hook.sh"
        capture_path = output / "command-hook-input.raw.json"
        marker_path = output / "command-hook-ran.marker"
        script.write_text(
            "#!/bin/sh\n"
            f"cat > {shlex.quote(str(capture_path))}\n"
            f"printf '%s\\n' 'FIXED_COMMAND_HOOK_STDOUT'\n"
            f"printf '%s\\n' 'FIXED_COMMAND_HOOK_STDERR' >&2\n"
            f"printf '%s\\n' 'ran' > {shlex.quote(str(marker_path))}\n"
            "exit 1\n",
            encoding="utf-8",
        )
        common_hook.update({"command": "/bin/sh", "args": [str(script)], "timeout": 5})
    elif case["family"] == "http-503":
        common_hook = {
            "type": "http",
            "url": f"http://127.0.0.1:{port}/hooks/failure",
            "timeout": 5,
        }
    else:
        raise ValueError(f"unknown failure family: {case['family']}")
    if case["on_failure"] is not None:
        common_hook["onFailure"] = case["on_failure"]
    return {"hooks": {"UserPromptSubmit": [{"hooks": [common_hook]}]}}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--case", required=True, choices=sorted(CASE_BY_ID))
    parser.add_argument("--binary", default=os.environ.get("CLAUDE_NATIVE_BINARY", str(DEFAULT_BINARY)))
    args = parser.parse_args()
    case = CASE_BY_ID[args.case]
    binary = Path(args.binary).expanduser().resolve()
    if not binary.is_file():
        raise SystemExit(f"Native executable does not exist: {binary}")
    version_run = subprocess.run(
        [str(binary), "--version"], capture_output=True, timeout=15, check=False)
    version_text = (version_run.stdout + version_run.stderr).decode(
        "utf-8", errors="backslashreplace").strip()
    binary_sha = sha256(binary.read_bytes())
    if version_run.returncode != 0 or FIXTURE["native_version"] not in version_text:
        raise SystemExit(f"Expected Native 2.1.295, received {version_text!r}")
    if binary_sha != FIXTURE["native_sha256"]:
        raise SystemExit(f"Native SHA mismatch: {binary_sha}")

    output = ROOT / "run-output" / case["id"]
    if output.exists():
        shutil.rmtree(output)
    output.mkdir(parents=True)
    dirs = {name: output / name for name in ("cwd", "home", "config", "tmp")}
    for path in dirs.values():
        path.mkdir()
    captured_requests: list[dict] = []
    captured_responses: list[dict] = []
    model_sse = make_sse(
        f"msg_native_hook_onfailure_295_{case['id']}",
        FIXTURE["model"],
        FIXTURE["main_response_text"],
    )
    http_failure_body = b'{"error":{"type":"fixed_hook_failure","message":"fixture HTTP 503"}}'

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def reply(self, status: int, body: bytes, content_type: str) -> None:
            self.send_response(status)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Content-Type", content_type)
            self.end_headers()
            self.wfile.write(body)
            self.wfile.flush()

        def do_POST(self) -> None:
            raw = self.rfile.read(int(self.headers.get("content-length", "0")))
            request_headers = list(self.headers.raw_items())
            sequence = len(captured_requests) + 1
            if self.path == "/v1/messages?beta=true":
                route = "anthropic_messages"
                accepted = (
                    self.client_address[0] == "127.0.0.1"
                    and self.headers.get("x-api-key") == FIXTURE["dummy_api_key"]
                )
                status, body, content_type = (
                    (200, model_sse, "text/event-stream")
                    if accepted else (404, b"{}", "application/json")
                )
            elif self.path == "/hooks/failure":
                route = "http_hook"
                accepted = self.client_address[0] == "127.0.0.1"
                status, body, content_type = (
                    (503, http_failure_body, "application/json")
                    if accepted else (404, b"{}", "application/json")
                )
            else:
                route = "unexpected"
                accepted = False
                status, body, content_type = 404, b"{}", "application/json"

            request_path = f"http-request-{sequence:02}.raw.body"
            (output / request_path).write_bytes(raw)
            captured_requests.append({
                "sequence": sequence,
                "route": route,
                "method": "POST",
                "path": self.path,
                "accepted_local_request": accepted,
                "status": status,
                "request_bytes": len(raw),
                "request_sha256": sha256(raw),
                "request_body": request_path,
                "headers_observed_in_order": request_headers,
                "response_bytes": len(body),
                "response_sha256": sha256(body),
            })
            response_path = f"http-response-{sequence:02}.raw.body"
            (output / response_path).write_bytes(body)
            captured_responses.append({
                "sequence": sequence,
                "route": route,
                "status": status,
                "content_type": content_type,
                "path": response_path,
                "bytes": len(body),
                "sha256": sha256(body),
            })
            self.reply(status, body, content_type)

    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    settings = settings_for(case, output, server.server_port)
    settings_path = output / "settings.json"
    settings_raw = json.dumps(settings, ensure_ascii=True, indent=2).encode("utf-8") + b"\n"
    settings_path.write_bytes(settings_raw)
    argv = [
        str(binary),
        "--setting-sources", "project",
        "--settings", str(settings_path),
        "--strict-mcp-config",
        "--print", "--output-format", "json", "--no-session-persistence",
        "--tools", "", "--max-turns", "1", "--max-budget-usd", "1",
        "--model", FIXTURE["model"], "--session-id", case["session_id"],
        "--debug", "hooks",
        FIXTURE["prompt"],
    ]
    env = {
        "PATH": "/usr/bin:/bin",
        "HOME": str(dirs["home"]),
        "TMPDIR": str(dirs["tmp"]),
        "LANG": "C.UTF-8",
        "TZ": "UTC",
        "TERM": "dumb",
        "CLAUDE_CONFIG_DIR": str(dirs["config"]),
        "ANTHROPIC_API_KEY": FIXTURE["dummy_api_key"],
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "ANTHROPIC_MAX_RETRIES": "0",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1",
        "DO_NOT_TRACK": "1",
    }
    try:
        try:
            completed = subprocess.run(
                argv, cwd=dirs["cwd"], env=env, capture_output=True,
                timeout=FIXTURE["timeout_seconds"], check=False)
            stdout, stderr = completed.stdout, completed.stderr
            native_exit_code, timed_out = completed.returncode, False
        except subprocess.TimeoutExpired as error:
            stdout, stderr = error.stdout or b"", error.stderr or b""
            native_exit_code, timed_out = None, True
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)

    (output / "native.stdout.raw").write_bytes(stdout)
    (output / "native.stderr.raw").write_bytes(stderr)
    parsed_stdout = None
    stdout_json_error = None
    try:
        parsed_stdout = json.loads(stdout)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        stdout_json_error = type(error).__name__
    marker_path = output / "command-hook-ran.marker"
    input_path = output / "command-hook-input.raw.json"
    hook_stdout_marked = b"FIXED_COMMAND_HOOK_STDOUT" in stderr or b"FIXED_COMMAND_HOOK_STDOUT" in stdout
    hook_stderr_marked = b"FIXED_COMMAND_HOOK_STDERR" in stderr or b"FIXED_COMMAND_HOOK_STDERR" in stdout
    receipt = {
        "native_version": version_text,
        "native_sha256": binary_sha,
        "binary_path": str(binary),
        "case": case,
        "on_failure_field_location": "individual hooks[] handler object; absent in default controls",
        "settings_sha256": sha256(settings_raw),
        "settings": settings,
        "public_entrypoint": "Native 2.1.295 CLI --print --output-format json with isolated UserPromptSubmit classic hook",
        "network_boundary": "127.0.0.1-only local HTTP server; Anthropic calls use dummy API key; HTTP-hook control returns fixed local 503",
        "retry_limit": 0,
        "timeout_seconds": FIXTURE["timeout_seconds"],
        "http_requests": captured_requests,
        "http_responses": captured_responses,
        "anthropic_messages_request_count": sum(item["route"] == "anthropic_messages" for item in captured_requests),
        "http_hook_request_count": sum(item["route"] == "http_hook" for item in captured_requests),
        "native_exit_code": native_exit_code,
        "timed_out": timed_out,
        "stdout": {"path": "native.stdout.raw", "bytes": len(stdout), "sha256": sha256(stdout)},
        "stdout_json_error": stdout_json_error,
        "stdout_result": parsed_stdout.get("result") if isinstance(parsed_stdout, dict) else None,
        "stdout_summary": ({key: parsed_stdout.get(key) for key in (
            "type", "subtype", "is_error", "session_id", "num_turns", "stop_reason"
        ) if key in parsed_stdout} if isinstance(parsed_stdout, dict) else None),
        "stderr": {"path": "native.stderr.raw", "bytes": len(stderr), "sha256": sha256(stderr)},
        "command_side_effects": {
            "hook_invocation_marker_exists": marker_path.exists(),
            "hook_stdin_capture_exists": input_path.exists(),
            "hook_stdin_capture_bytes": input_path.stat().st_size if input_path.exists() else None,
            "command_stdout_marker_visible_in_native_streams": hook_stdout_marked,
            "command_stderr_marker_visible_in_native_streams": hook_stderr_marked,
        },
        "scope": "Native 2.1.295 classic UserPromptSubmit command/HTTP failure onFailure probe; compares only the selected failure case against absent onFailure",
    }
    (output / "receipt.json").write_text(
        json.dumps(receipt, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({
        "case": case["id"],
        "on_failure": case["on_failure"],
        "native_exit_code": native_exit_code,
        "timed_out": timed_out,
        "anthropic_messages_request_count": receipt["anthropic_messages_request_count"],
        "http_hook_request_count": receipt["http_hook_request_count"],
        "stdout_summary": receipt["stdout_summary"],
        "command_side_effects": receipt["command_side_effects"],
        "receipt": str((output / "receipt.json").relative_to(ROOT)),
    }, ensure_ascii=True, indent=2))
    return 1 if timed_out else 0


if __name__ == "__main__":
    raise SystemExit(main())
