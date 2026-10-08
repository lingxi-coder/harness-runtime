#!/usr/bin/env python3
"""Bounded Native 2.1.293 retry-control probe with x-should-retry:false."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import threading


ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "run-output"
BINARY_VALUE = os.environ.get("CLAUDE_NATIVE_BINARY")
EXPECTED_SHA256 = "4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e"
DUMMY_KEY = "native-provider-retry-header-fixture-dummy-key"
CASES = json.loads((ROOT / "retry-header-controls.json").read_text(encoding="utf-8"))
BODY_CASES = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))["cases"]
BODY = bytes.fromhex(
    next(case["response_body_hex"] for case in BODY_CASES if case["id"] == CASES["response_body_case"])
)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def utf16_units(text: str) -> list[int]:
    raw = text.encode("utf-16-le", errors="surrogatepass")
    return [int.from_bytes(raw[index:index + 2], "little") for index in range(0, len(raw), 2)]


def main() -> None:
    if not BINARY_VALUE:
        raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
    binary = Path(BINARY_VALUE).expanduser().resolve()
    binary_sha = sha256(binary.read_bytes())
    if binary_sha != EXPECTED_SHA256:
        raise SystemExit(f"Native SHA mismatch: {binary_sha}")
    version = subprocess.run([str(binary), "--version"], capture_output=True, timeout=15, check=False)
    version_text = (version.stdout + version.stderr).decode("utf-8", errors="backslashreplace").strip()
    if version.returncode != 0 or not version_text.startswith("2.1.293 "):
        raise SystemExit(f"Native version mismatch: {version_text!r}")

    OUTPUT.mkdir(parents=True, exist_ok=True)
    run_root = Path(tempfile.mkdtemp(prefix="retry-header-controls-", dir=OUTPUT))
    active_case = [None]
    requests: list[dict] = []

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def reply(self, status: int, body: bytes = b"") -> None:
            self.send_response(status)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Content-Type", "application/json")
            for name, value in CASES["response_header"].items():
                self.send_header(name, value)
            self.end_headers()
            if body:
                self.wfile.write(body)

        def do_HEAD(self) -> None:
            accepted = self.client_address[0] == "127.0.0.1" and self.path == "/api/hello"
            requests.append({"case": active_case[0], "method": "HEAD", "path": self.path, "accepted": accepted})
            self.reply(200 if accepted else 404)

        def do_POST(self) -> None:
            raw = self.rfile.read(int(self.headers.get("content-length", "0")))
            accepted = (
                self.client_address[0] == "127.0.0.1"
                and self.path == "/v1/messages?beta=true"
                and self.headers.get("x-api-key") == DUMMY_KEY
            )
            requests.append({
                "case": active_case[0],
                "method": "POST",
                "path": self.path,
                "accepted": accepted,
                "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                "request_bytes": len(raw),
                "request_sha256": sha256(raw),
                "response_status": CASES_BY_ID[active_case[0]]["http_status"],
            })
            self.reply(CASES_BY_ID[active_case[0]]["http_status"], BODY)

    CASES_BY_ID = {case["id"]: case for case in CASES["cases"]}
    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    results = []
    try:
        for case in CASES["cases"]:
            active_case[0] = case["id"]
            case_root = run_root / case["id"]
            for name in ("cwd", "home", "config", "tmp"):
                (case_root / name).mkdir(parents=True, exist_ok=True)
            env = {
                "PATH": "/usr/bin:/bin",
                "HOME": str(case_root / "home"),
                "TMPDIR": str(case_root / "tmp"),
                "LANG": "C.UTF-8",
                "TZ": "UTC",
                "TERM": "dumb",
                "CLAUDE_CONFIG_DIR": str(case_root / "config"),
                "ANTHROPIC_API_KEY": DUMMY_KEY,
                "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
                "DISABLE_TELEMETRY": "1",
                "DO_NOT_TRACK": "1",
            }
            argv = [
                str(binary), "--setting-sources", "project", "--strict-mcp-config",
                "--print", "--output-format", "json", "--no-session-persistence",
                "--tools", "", "--max-budget-usd", "1", "--max-turns", "1",
                "--model", CASES["model"], "--session-id", case["session_id"], CASES["prompt"],
            ]
            try:
                completed = subprocess.run(
                    argv, cwd=case_root / "cwd", env=env, capture_output=True, timeout=10, check=False
                )
                stdout, stderr = completed.stdout, completed.stderr
                exit_code, timed_out = completed.returncode, False
            except subprocess.TimeoutExpired as error:
                stdout, stderr = error.stdout or b"", error.stderr or b""
                exit_code, timed_out = None, True
            (case_root / "native.stdout.raw").write_bytes(stdout)
            (case_root / "native.stderr.raw").write_bytes(stderr)
            native_json = None
            try:
                native_json = json.loads(stdout)
            except (UnicodeDecodeError, json.JSONDecodeError):
                pass
            native_result = native_json.get("result") if isinstance(native_json, dict) else None
            result_units = utf16_units(native_result) if isinstance(native_result, str) else []
            if isinstance(native_result, str) and native_result.startswith("Failed to authenticate."):
                text_shape = "authentication_notice"
            elif isinstance(native_result, str) and native_result.startswith("API Error:") and "This is a server-side issue" in native_result:
                text_shape = "server_error_notice"
            elif isinstance(native_result, str) and native_result.startswith("API Error:"):
                text_shape = "api_error_prefix"
            else:
                text_shape = "other_or_unavailable"
            post_requests = [request for request in requests if request["case"] == case["id"] and request["method"] == "POST"]
            result = {
                "id": case["id"],
                "http_status": case["http_status"],
                "response_headers": CASES["response_header"],
                "post_count": len(post_requests),
                "requests": post_requests,
                "timed_out": timed_out,
                "exit_code": exit_code,
                "stdout_bytes": len(stdout),
                "stdout_sha256": sha256(stdout),
                "stdout_path": f"{case['id']}/native.stdout.raw",
                "stderr_bytes": len(stderr),
                "stderr_sha256": sha256(stderr),
                "stderr_path": f"{case['id']}/native.stderr.raw",
                "native_json": ({
                    key: native_json.get(key)
                    for key in ("type", "subtype", "is_error", "terminal_reason", "api_error_status", "num_turns")
                    if key in native_json
                } if isinstance(native_json, dict) else None),
                "result_text_shape": text_shape if isinstance(native_result, str) else None,
                "result_utf16_units": len(result_units) if isinstance(native_result, str) else None,
                "result_lone_surrogate_indices": [index for index, unit in enumerate(result_units) if 0xD800 <= unit <= 0xDFFF],
                "result_starts_with_api_error": native_result.startswith("API Error:") if isinstance(native_result, str) else None,
                "result_starts_with_authentication_notice": native_result.startswith("Failed to authenticate.") if isinstance(native_result, str) else None,
                "result_has_server_side_notice": "This is a server-side issue" in native_result if isinstance(native_result, str) else None,
                "result_has_dynamic_loopback_endpoint": bool(re.search(r"127\.0\.0\.1:\d+", native_result)) if isinstance(native_result, str) else None,
            }
            results.append(result)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)

    receipt = {
        "native_version": version_text,
        "native_sha256": binary_sha,
        "runner_sha256": sha256(Path(__file__).read_bytes()),
        "fixture_sha256": {
            "retry-header-controls.json": sha256((ROOT / "retry-header-controls.json").read_bytes()),
            "cases.json": sha256((ROOT / "cases.json").read_bytes()),
        },
        "run_output_dir": str(run_root.relative_to(ROOT)),
        "loopback_only": True,
        "dummy_key_only": True,
        "max_retry_env_set": False,
        "response_header_control": CASES["response_header"],
        "results": results,
        "scope": "x-should-retry:false control probe only; does not establish default retry policy.",
    }
    (run_root / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n")
    print(json.dumps(receipt, ensure_ascii=True, indent=2))


if __name__ == "__main__":
    main()
