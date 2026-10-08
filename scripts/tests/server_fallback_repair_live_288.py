#!/usr/bin/env python3
"""Probe shared native fallback repair across Anthropic-family HTTP wrappers.

All credentials are fake and every provider endpoint is a local loopback mock.
This does not exercise signed AWS/GCP/Azure requests or a live provider.
"""
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from compact_live_oracle import isolated_env

NATIVE_SHA256 = "bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750"
ERROR = {
    "type": "error",
    "error": {
        "type": "invalid_request_error",
        "message": "Invalid beta header `server-side-fallback-2026-07-01` for anthropic-beta",
    },
}
FIXTURE = Path(__file__).resolve().parents[2] / "crates/llm-runtime/tests/fixtures/server_fallback_repair_live_2_1_288.json"


def public_request(method: str, path: str, headers: dict[str, str], body: object, status: int) -> dict:
    """Keep routing evidence while excluding credentials and per-run metadata."""
    keep_headers = {"anthropic-beta", "anthropic-version", "content-type", "x-amzn-bedrock-accept"}
    normalized = {name.lower(): value for name, value in headers.items()}
    body = body if isinstance(body, dict) else {}
    return {
        "method": method,
        "path": path,
        "status": status,
        "headers": {name: normalized[name] for name in sorted(keep_headers) if name in normalized},
        "model": body.get("model"),
        "stream": body.get("stream"),
        "fallbacks": body.get("fallbacks"),
        "anthropic_version": body.get("anthropic_version"),
        "anthropic_beta": body.get("anthropic_beta"),
    }


def run_scenario(binary: Path, name: str) -> tuple[dict, Path]:
    root = Path(tempfile.mkdtemp(prefix=f"harness-288-fallback-repair-{name}-"))
    for child in ("config", "work", "tmp"):
        (root / child).mkdir()
    rows: list[dict] = []
    lock = threading.Lock()

    class Mock(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def reject(self):
            # Any proxy escape remains on this local server and is denied.
            self.send_error(403, "Only the loopback oracle endpoint is available")

        do_CONNECT = reject

        def record_and_reject(self):
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            try:
                body = json.loads(raw) if raw else None
            except json.JSONDecodeError:
                body = None
            with lock:
                rows.append(public_request(self.command, self.path, dict(self.headers.items()), body, 400))
            encoded = json.dumps(ERROR).encode()
            self.send_response(400)
            self.send_header("Content-Type", "application/json")
            self.send_header("x-should-retry", "false")
            self.send_header("Content-Length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)

        do_GET = record_and_reject
        do_POST = record_and_reject

    server = ThreadingHTTPServer(("127.0.0.1", 0), Mock)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    base_url = f"http://127.0.0.1:{server.server_port}"
    env = isolated_env(root, base_url)
    env["CLAUDE_CODE_MAX_RETRIES"] = "0"

    if name == "first_party":
        env.update(ANTHROPIC_API_KEY="sk-ant-api03-fake-local-only", ANTHROPIC_BASE_URL=base_url)
        api_route = lambda row: row["path"].split("?", 1)[0] == "/v1/messages"
    elif name == "bedrock":
        env.update(
            CLAUDE_CODE_USE_BEDROCK="1",
            CLAUDE_CODE_SKIP_BEDROCK_AUTH="1",
            ANTHROPIC_BEDROCK_BASE_URL=base_url,
            AWS_REGION="us-east-1",
            AWS_ACCESS_KEY_ID="fixture-access-key",
            AWS_SECRET_ACCESS_KEY="fixture-secret-key",
        )
        api_route = lambda row: "/invoke-with-response-stream" in row["path"]
    elif name == "vertex":
        env.update(
            CLAUDE_CODE_USE_VERTEX="1",
            CLAUDE_CODE_SKIP_VERTEX_AUTH="1",
            ANTHROPIC_VERTEX_BASE_URL=base_url,
            ANTHROPIC_VERTEX_PROJECT_ID="fixture-project",
            CLOUD_ML_REGION="us-east5",
        )
        api_route = lambda row: row["path"].endswith(":streamRawPredict")
    elif name == "foundry":
        # Native Foundry rejects baseURL+resource as mutually exclusive.
        env.update(
            CLAUDE_CODE_USE_FOUNDRY="1",
            ANTHROPIC_FOUNDRY_BASE_URL=base_url,
            ANTHROPIC_FOUNDRY_API_KEY="fake-foundry-key-local-only",
        )
        api_route = lambda row: row["path"].split("?", 1)[0] == "/v1/messages"
    else:
        raise AssertionError(name)

    command = [
        str(binary), "--bare", "-p", "server fallback repair probe",
        "--model", "claude-sonnet-4-6", "--system-prompt", "fixture",
        "--output-format", "stream-json", "--verbose", "--setting-sources", "",
        "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}', "--tools", "",
        "--debug-file", str(root / "debug.log"),
    ]
    process = subprocess.Popen(
        command, cwd=root / "work", env=env, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, start_new_session=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=45)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            stdout, stderr = process.communicate(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            stdout, stderr = process.communicate(timeout=3)
        raise AssertionError(f"native {name} fallback repair exceeded the bounded probe")
    finally:
        server.shutdown()
        server.server_close()

    (root / "stdout.jsonl").write_bytes(stdout)
    (root / "stderr.txt").write_bytes(stderr)
    debug = (root / "debug.log").read_text(errors="replace")
    repair_logs = [
        line.split(" [WARN] ", 1)[-1]
        for line in debug.splitlines()
        if "[server-fallback] 400 attributed (category_beta_header) — stripping and retrying" in line
    ]
    api_rows = [row for row in rows if api_route(row)]

    assert process.returncode == 1, (name, process.returncode, debug[-1200:])
    assert len(repair_logs) == 1, (name, repair_logs, debug[-1600:])
    assert len(api_rows) >= 2, (name, api_rows, debug[-1600:])
    assert all(row["status"] == 400 for row in api_rows)

    return {
        "provider": name,
        "exit_code": process.returncode,
        "repair_retry_logs": repair_logs,
        "api_request_count": len(api_rows),
        "requests": rows,
    }, root


def main() -> None:
    binary = Path(sys.argv[1])
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == NATIVE_SHA256
    scenarios = []
    log_dirs = []
    for name in ("first_party", "bedrock", "vertex", "foundry"):
        result, root = run_scenario(binary, name)
        scenarios.append(result)
        log_dirs.append(str(root))

    fixture = {
        "version": "2.1.288",
        "native_sha256": NATIVE_SHA256,
        "scope": "Actual native CLI over localhost-only HTTP mocks using fake credentials. Each provider receives the same 400 category-beta error and must log the shared r3e retry. No signed cloud request or live provider is exercised.",
        "scenarios": scenarios,
    }
    output = json.dumps(fixture, indent=2, ensure_ascii=False) + "\n"
    if "--check" in sys.argv:
        assert FIXTURE.read_text() == output
        mode = "checked"
    else:
        FIXTURE.parent.mkdir(parents=True, exist_ok=True)
        FIXTURE.write_text(output)
        mode = "wrote"
    print(json.dumps({"mode": mode, "providers": [x["provider"] for x in scenarios],
                      "log_dirs": log_dirs, "fixture": str(FIXTURE)}))


if __name__ == "__main__":
    main()
