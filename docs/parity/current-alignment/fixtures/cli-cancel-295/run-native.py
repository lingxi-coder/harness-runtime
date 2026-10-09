#!/usr/bin/env python3
"""Observe Native CLI SIGINT during a single in-flight loopback request."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time


ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "run-output"
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
BINARY_VALUE = os.environ.get("CLAUDE_NATIVE_BINARY")
CWD_VALUE = os.environ.get("CLAUDE_CANCEL_FIXTURE_CWD")
EXPECTED_SHA256 = "0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70"
DUMMY_KEY = "native-cli-cancel-fixture-dummy-key"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def utf16_units(text: str) -> list[int]:
    raw = text.encode("utf-16-le", errors="surrogatepass")
    return [int.from_bytes(raw[index:index + 2], "little") for index in range(0, len(raw), 2)]


def main() -> None:
    if not BINARY_VALUE:
        raise SystemExit("Set CLAUDE_NATIVE_BINARY to the pinned Native executable")
    if not CWD_VALUE:
        raise SystemExit("Set CLAUDE_CANCEL_FIXTURE_CWD to a fixed empty directory outside this repository")

    binary = Path(BINARY_VALUE).expanduser().resolve()
    binary_sha = sha256(binary.read_bytes())
    if binary_sha != EXPECTED_SHA256:
        raise SystemExit(f"Native SHA mismatch: {binary_sha}")
    version = subprocess.run([str(binary), "--version"], capture_output=True, timeout=15, check=False)
    version_text = (version.stdout + version.stderr).decode("utf-8", errors="backslashreplace").strip()
    if version.returncode != 0 or not version_text.startswith("2.1.295 "):
        raise SystemExit(f"Native version mismatch: {version_text!r}")

    repo_root = ROOT.parents[4]
    cwd = Path(CWD_VALUE).expanduser().resolve()
    try:
        cwd.relative_to(repo_root.parents[2])
    except ValueError:
        pass
    else:
        raise SystemExit("CLAUDE_CANCEL_FIXTURE_CWD must be outside the LingXi workspace")
    cwd.mkdir(parents=True, exist_ok=True)
    if any(cwd.iterdir()):
        raise SystemExit(f"Refusing a non-empty cancellation fixture CWD: {cwd}")

    OUTPUT.mkdir(parents=True, exist_ok=True)
    run_root = Path(tempfile.mkdtemp(prefix="sigint-", dir=OUTPUT))
    for name in ("home", "config", "tmp"):
        (run_root / name).mkdir()
    post_seen = threading.Event()
    release_stub = threading.Event()
    records: list[dict] = []

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def reply(self, status: int) -> None:
            self.send_response(status)
            self.send_header("Content-Length", "0")
            self.end_headers()

        def do_HEAD(self) -> None:
            accepted = self.client_address[0] == "127.0.0.1" and self.path == "/api/hello"
            records.append({"method": "HEAD", "path": self.path, "accepted": accepted})
            self.reply(200 if accepted else 404)

        def do_POST(self) -> None:
            raw = self.rfile.read(int(self.headers.get("content-length", "0")))
            accepted = (
                self.client_address[0] == "127.0.0.1"
                and self.path == "/v1/messages?beta=true"
                and self.headers.get("x-api-key") == DUMMY_KEY
            )
            record = {
                "method": "POST",
                "path": self.path,
                "accepted": accepted,
                "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                "request_bytes": len(raw),
                "request_sha256": sha256(raw),
                "stream": json.loads(raw).get("stream"),
            }
            records.append(record)
            (run_root / f"request-{len(records):02}.raw.json").write_bytes(raw)
            if not accepted:
                self.reply(401)
                return
            post_seen.set()
            # Keep the provider call in-flight until SIGINT has been delivered.
            release_stub.wait(12)

    server = HTTPServer(("127.0.0.1", 0), Handler)
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    env = {
        "PATH": "/usr/bin:/bin",
        "TMPDIR": str(run_root / "tmp"),
        "LANG": "C.UTF-8",
        "TZ": "UTC",
        "TERM": "dumb",
        "HOME": str(run_root / "home"),
        "CLAUDE_CONFIG_DIR": str(run_root / "config"),
        "ANTHROPIC_API_KEY": DUMMY_KEY,
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "ANTHROPIC_MAX_RETRIES": "0",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1",
        "DO_NOT_TRACK": "1",
    }
    args = [
        "--setting-sources", "project", "--strict-mcp-config", "--print",
        "--output-format", "json", "--tools", "", "--max-turns", "1",
        "--model", FIXTURE["model"], "--session-id", FIXTURE["session_id"],
        "--debug", "hooks", FIXTURE["prompt"],
    ]
    started = time.monotonic()
    process = subprocess.Popen(
        [str(binary), *args], cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE
    )
    signal_sent = False
    cleanup_signal = None
    timed_out = False
    try:
        if not post_seen.wait(30):
            raise RuntimeError("Native did not reach the single loopback POST within 30 seconds")
        process.send_signal(signal.SIGINT)
        signal_sent = True
        try:
            stdout, stderr = process.communicate(timeout=8)
        except subprocess.TimeoutExpired:
            timed_out = True
            process.send_signal(signal.SIGTERM)
            cleanup_signal = "SIGTERM"
            try:
                stdout, stderr = process.communicate(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                cleanup_signal = "SIGKILL"
                stdout, stderr = process.communicate(timeout=3)
    finally:
        release_stub.set()
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=2)
        if process.poll() is None:
            process.kill()
            process.wait(timeout=3)

    (run_root / "stdout.raw").write_bytes(stdout)
    (run_root / "stderr.raw").write_bytes(stderr)
    try:
        stdout_value = json.loads(stdout)
    except (UnicodeDecodeError, json.JSONDecodeError):
        stdout_value = None
    session_files = list((run_root / "config").glob(f"projects/*/{FIXTURE['session_id']}.jsonl"))
    if len(session_files) > 1:
        raise SystemExit(f"expected at most one fixed Native session file, found {len(session_files)}")
    session_raw = session_files[0].read_bytes() if session_files else b""
    if session_files:
        (run_root / "session.raw.jsonl").write_bytes(session_raw)

    rows = []
    for index, line in enumerate(session_raw.splitlines()):
        try:
            row = json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError):
            rows.append({"index": index, "json_parse": "failed"})
            continue
        message = row.get("message") if isinstance(row, dict) else None
        text_units = []
        content = message.get("content") if isinstance(message, dict) else None
        if isinstance(content, str):
            text_units = [utf16_units(content)]
        elif isinstance(content, list):
            text_units = [
                utf16_units(block["text"])
                for block in content
                if isinstance(block, dict) and block.get("type") == "text" and isinstance(block.get("text"), str)
            ]
        rows.append({
            "index": index,
            "type": row.get("type") if isinstance(row, dict) else None,
            "session_id": row.get("sessionId") if isinstance(row, dict) else None,
            "role": message.get("role") if isinstance(message, dict) else None,
            "text_units": text_units,
        })

    prompt_units = utf16_units(FIXTURE["prompt"])
    interrupt_units = utf16_units("[Request interrupted by user]")
    user_text_rows = [row for row in rows if row["role"] == "user" and row["text_units"]]
    interrupt_rows = [row for row in user_text_rows if interrupt_units in row["text_units"]]
    prompt_rows = [row for row in user_text_rows if prompt_units in row["text_units"]]
    stdout_summary = None
    if isinstance(stdout_value, dict):
        stdout_summary = {
            key: stdout_value.get(key)
            for key in ("type", "is_error", "terminal_reason", "subtype", "stop_reason", "num_turns", "session_id")
        }
        if isinstance(stdout_value.get("errors"), list):
            stdout_summary["errors"] = stdout_value["errors"]

    receipt = {
        "native_version": version_text,
        "native_sha256": binary_sha,
        "runner_sha256": sha256(Path(__file__).read_bytes()),
        "fixture_sha256": sha256((ROOT / "cases.json").read_bytes()),
        "fixed_inputs": {key: FIXTURE[key] for key in ("session_id", "model", "prompt", "signal", "max_retries")},
        "network_boundary": "127.0.0.1 loopback only; isolated HOME/config; dummy key; stub withheld its response until after SIGINT",
        "requests": records,
        "stub_response_sent": False,
        "sigint_sent_after_request": signal_sent,
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "native_exit_code": process.returncode,
        "timed_out_after_sigint": timed_out,
        "cleanup_signal": cleanup_signal,
        "stdout": {"path": "stdout.raw", "bytes": len(stdout), "sha256": sha256(stdout)},
        "stderr": {"path": "stderr.raw", "bytes": len(stderr), "sha256": sha256(stderr)},
        "stdout_result": stdout_summary,
        "session": {
            "present": bool(session_files),
            "path": "session.raw.jsonl" if session_files else None,
            "bytes": len(session_raw),
            "sha256": sha256(session_raw),
            "rows": rows,
            "fixed_prompt_user_rows": len(prompt_rows),
            "interrupt_user_rows": len(interrupt_rows),
            "assistant_rows": sum(row["role"] == "assistant" for row in rows),
        },
        "checks": {
            "all_session_rows_use_bare_uuid": bool(rows) and all(row.get("session_id") == FIXTURE["session_id"] for row in rows if row.get("session_id") is not None),
            "one_accepted_dummy_key_request": sum(r.get("method") == "POST" and r.get("accepted") and r.get("dummy_key_matched") for r in records) == 1,
            "sigint_was_sent": signal_sent,
            "native_exited_without_forced_cleanup": not timed_out,
            "json_result_is_aborted_streaming_error": isinstance(stdout_summary, dict) and stdout_summary.get("type") == "result" and stdout_summary.get("is_error") is True and stdout_summary.get("terminal_reason") == "aborted_streaming" and stdout_summary.get("subtype") == "error_during_execution",
            "session_keeps_prompt_then_interrupt": len(prompt_rows) == 1 and len(interrupt_rows) == 1 and prompt_rows[0]["index"] < interrupt_rows[0]["index"],
        },
        "scope": "One public Native 2.1.295 CLI process, fixed session ID and prompt, one in-flight loopback Anthropic Messages request, then SIGINT. This observes the CLI interruption/output/persistence boundary only; it does not prove Harness turn cancellation-token parity, tool side-effect rollback, retry behavior without the explicit zero-retry setting, or Native module unload.",
    }
    (run_root / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(receipt, ensure_ascii=True, indent=2))
    if not all(receipt["checks"].values()):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
