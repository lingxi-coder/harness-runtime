#!/usr/bin/env python3
"""Public Native Bash input repair through hooks, execution and model replay."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import uuid
from http.server import BaseHTTPRequestHandler, HTTPServer

ROOT = Path(__file__).resolve().parent
BINARY = Path(os.environ["CLAUDE_NATIVE_BINARY"]).resolve()
SHA = "0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70"
KEY = "native-bash-input-295-dummy-key"
CASES = {
    "alias-string": {"command_description": "Fixed alias description"},
    "existing-description": {"command_description": "Ignored alias", "description": "Existing description"},
    "nonstring-alias": {"command_description": None},
    "lone-surrogate-alias": {"command_description": "source-\ud800-tail"},
    "combined-timeout-alias": {"timeout_ms": "5000", "command_description": "Combined aliases"},
}

def digest(raw):
    return hashlib.sha256(raw).hexdigest()

def response(index, tool_input):
    events = [("message_start", {"type": "message_start", "message": {
        "id": f"msg_bash_input_295_{index}", "type": "message", "role": "assistant",
        "model": "claude-sonnet-5-5", "content": [], "stop_reason": None,
        "stop_sequence": None, "usage": {"input_tokens": 3, "output_tokens": 0}}})]
    if index == 1:
        block = {"type": "tool_use", "id": "toolu_bash_input_295", "name": "Bash", "input": {}}
        delta = {"type": "input_json_delta", "partial_json": json.dumps(tool_input, ensure_ascii=True, separators=(",", ":"))}
    else:
        block = {"type": "text", "text": ""}
        delta = {"type": "text_delta", "text": "BASH_INPUT_FIXTURE_COMPLETE"}
    events += [("content_block_start", {"type": "content_block_start", "index": 0, "content_block": block}),
               ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": delta}),
               ("content_block_stop", {"type": "content_block_stop", "index": 0}),
               ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use" if index == 1 else "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 2}}),
               ("message_stop", {"type": "message_stop"})]
    return "".join(f"event: {name}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n" for name, value in events).encode()

def run_case(name, extras, root):
    case = root / name
    for directory in ("cwd", "config", "home", "tmp"):
        (case / directory).mkdir(parents=True)
    tool_input = {"command": "printf BASH_295_ALIAS > probe.ok", **extras}
    settings = {"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "cat > hook-input.json"}]}]}}
    (case / "settings.json").write_text(json.dumps(settings))
    requests = []
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass
        def do_POST(self):
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            accepted = self.path == "/v1/messages?beta=true" and self.headers.get("x-api-key") == KEY
            index = len(requests) + 1
            requests.append({"index": index, "accepted": accepted, "sha256": digest(raw)})
            (case / f"request-{index:02}.raw.json").write_bytes(raw)
            body = response(index, tool_input) if accepted and index <= 2 else b"unexpected request"
            (case / f"response-{index:02}.raw.sse").write_bytes(body)
            self.send_response(200 if accepted and index <= 2 else 400)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
    server = HTTPServer(("127.0.0.1", 0), Handler)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    env = {"PATH": "/usr/bin:/bin", "HOME": str(case / "home"), "TMPDIR": str(case / "tmp"),
           "CLAUDE_CONFIG_DIR": str(case / "config"), "LANG": "C.UTF-8", "TZ": "UTC", "TERM": "dumb",
           "ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
           "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "DISABLE_TELEMETRY": "1", "DO_NOT_TRACK": "1"}
    session = str(uuid.uuid5(uuid.NAMESPACE_URL, "claude-alignment-bash-295/" + name))
    argv = [str(BINARY), "--setting-sources", "project", "--strict-mcp-config", "--print", "--tools", "Bash",
            "--allowedTools", "Bash", "--model", "claude-sonnet-5-5", "--output-format", "json", "--session-id", session,
            "--settings", str(case / "settings.json"), "--system-prompt", "Use the supplied Bash call, then finish.", "Fixed Bash fixture."]
    try:
        result = subprocess.run(argv, cwd=case / "cwd", env=env, capture_output=True, timeout=45)
    finally:
        server.shutdown()
        server.server_close()
        worker.join()
    baseline = root / "curated" / "2.1.295" / name
    baseline.mkdir(parents=True, exist_ok=True)
    (baseline / "stdout.raw.json").write_bytes(result.stdout)
    (baseline / "stderr.raw.txt").write_bytes(result.stderr)
    for index in range(1, len(requests) + 1):
        (baseline / f"response-{index:02}.raw.sse").write_bytes((case / f"response-{index:02}.raw.sse").read_bytes())
    hook = case / "cwd" / "hook-input.json"
    if hook.exists():
        (baseline / "hook-input.raw.json").write_bytes(hook.read_bytes())
    marker = case / "cwd" / "probe.ok"
    receipt = {"case": name, "native_version": "2.1.295", "native_sha256": SHA, "session_id": session,
               "run_directory": str(case), "exit_code": result.returncode, "requests": requests,
               "tool_input": tool_input, "hook_ran": hook.exists(), "marker_bytes": marker.read_text() if marker.exists() else None,
               "observed_hook_tool_input": json.loads(hook.read_bytes()).get("tool_input") if hook.exists() else None}
    (baseline / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n")
    print(json.dumps(receipt, ensure_ascii=True), flush=True)
    assert result.returncode == 0 and len(requests) == 2 and all(r["accepted"] for r in requests)
    assert marker.read_bytes() == b"BASH_295_ALIAS"

def main():
    assert digest(BINARY.read_bytes()) == SHA
    assert subprocess.check_output([str(BINARY), "--version"]).decode().strip() == "2.1.295 (Claude Code)"
    root = Path(tempfile.mkdtemp(prefix="claude-alignment-native-bash-295-", dir="/private/tmp"))
    for name, extras in CASES.items():
        run_case(name, extras, root)

if __name__ == "__main__":
    main()
