#!/usr/bin/env python3
"""Replay frozen Claude 2.1.295 timing responses through the Product CLI."""

from __future__ import annotations

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import argparse
import hashlib
import io
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import time
from http.client import parse_headers


ROOT = Path(__file__).resolve().parent
TIMED_ROOT = ROOT.parent
HARNESS_FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
TIMED_CASES = json.loads((TIMED_ROOT / "cases.json").read_text(encoding="utf-8"))
TIMED_RECEIPT = json.loads(
    (TIMED_ROOT / "native-baseline/2.1.295/receipt.json").read_text(encoding="utf-8")
)
FALLBACK_RECEIPT = json.loads(
    (TIMED_ROOT / "native-baseline/2.1.295/edge-cases/http404-nonstream-fallback/receipt.json").read_text(encoding="utf-8")
)
OUTPUT = ROOT / "run-output"
DUMMY_KEY = "product-timed-sse-295-loopback-dummy-key"
OUTPUT.mkdir(parents=True, exist_ok=True)


def sha256(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


class RecordingReader:
    """Record exact request-line/header bytes while retaining file-like reads."""

    def __init__(self, wrapped):
        self.wrapped = wrapped
        self.blocks: list[bytes] = []
        self.collecting = False
        self.current = bytearray()

    def readline(self, *args):
        if not self.collecting:
            self.collecting = True
            self.current = bytearray()
        line = self.wrapped.readline(*args)
        self.current.extend(line)
        if line in (b"\r\n", b"\n", b""):
            self.blocks.append(bytes(self.current))
            self.collecting = False
        return line

    def read(self, *args):
        return self.wrapped.read(*args)

    def __getattr__(self, name):
        return getattr(self.wrapped, name)


def sse_chunks(raw: bytes, expected_count: int) -> list[bytes]:
    parts = raw.split(b"\n\n")
    if parts and parts[-1] == b"":
        parts.pop()
    chunks = [part + b"\n\n" for part in parts]
    if len(chunks) != expected_count:
        raise ValueError(f"SSE event count mismatch: expected {expected_count}, got {len(chunks)}")
    return chunks


def send_headers(handler: BaseHTTPRequestHandler, status_line: bytes, headers: list[tuple[bytes, bytes]]) -> bytes:
    raw = status_line + b"\r\n"
    for name, value in headers:
        raw += name + b": " + value + b"\r\n"
    raw += b"\r\n"
    handler.wfile.write(raw)
    handler.wfile.flush()
    return raw


def run_case(case: dict, binary: Path, product_revision: str | None) -> dict:
    run_root = Path(tempfile.mkdtemp(prefix=f"product-timed-295-{case['id']}-", dir=OUTPUT))
    dirs = {name: run_root / name for name in ("home", "config", "tmp", "cwd")}
    for path in dirs.values():
        path.mkdir()

    fallback_case = next(item for item in TIMED_CASES["cases"] if item["id"] == "http404-nonstream-fallback")
    fallback_root = TIMED_ROOT / "native-baseline/2.1.295/edge-cases/http404-nonstream-fallback"
    error_body = (fallback_root / "http-response-01.raw.json").read_bytes()
    nonstream_body = (fallback_root / "planned-nonstream-message.raw.json").read_bytes()
    timed_root = TIMED_ROOT / "native-baseline/2.1.295"
    sse_body = (timed_root / "scheduled-events.raw.sse").read_bytes()
    sse_events = TIMED_CASES["events"]
    chunks = sse_chunks(sse_body, len(sse_events))

    request_records: list[dict] = []
    response_records: list[dict] = []
    request_lock = threading.Lock()
    process_start_ns = [0]

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def setup(self) -> None:
            super().setup()
            self.rfile = RecordingReader(self.rfile)

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def do_POST(self) -> None:
            received_ns = time.monotonic_ns()
            try:
                content_length = int(self.headers.get("content-length", "0"))
            except ValueError:
                content_length = 0
            body = self.rfile.read(content_length)
            try:
                parsed = json.loads(body)
            except (UnicodeDecodeError, json.JSONDecodeError):
                parsed = None
            with request_lock:
                sequence = len(request_records) + 1
                request_headers = self.rfile.blocks[-1] if self.rfile.blocks else b""
                request_header_path = f"request-{sequence:02}.headers.raw"
                request_body_path = f"request-{sequence:02}.body.raw"
                (run_root / request_header_path).write_bytes(request_headers)
                (run_root / request_body_path).write_bytes(body)
                accepted = (
                    self.client_address[0] == "127.0.0.1"
                    and self.command == "POST"
                    and self.path == "/v1/messages?beta=true"
                    and self.headers.get("x-api-key") == DUMMY_KEY
                )
                request_records.append({
                    "sequence": sequence,
                    "accepted_loopback_dummy_key": accepted,
                    "method": self.command,
                    "path": self.path,
                    "headers_path": request_header_path,
                    "headers_bytes": len(request_headers),
                    "headers_sha256": sha256(request_headers),
                    "headers_raw_items": list(self.headers.raw_items()),
                    "body_path": request_body_path,
                    "body_bytes": len(body),
                    "body_sha256": sha256(body),
                    "stream": parsed.get("stream") if isinstance(parsed, dict) else None,
                    "model": parsed.get("model") if isinstance(parsed, dict) else None,
                    "received_since_product_spawn_ms": round((received_ns - process_start_ns[0]) / 1_000_000, 3),
                })

            if case["mode"] == "sse":
                status, response_kind, response_body = 200, "sse", sse_body
            elif case["mode"] == "fallback-404" and sequence == 1:
                status, response_kind, response_body = 404, "native_404_error_body", error_body
                time.sleep(fallback_case["first_response_delay_ms"] / 1000)
            elif case["mode"] == "fallback-404":
                status, response_kind, response_body = 200, "native_nonstream_anthropic_message", nonstream_body
                time.sleep(fallback_case["fallback_response_delay_ms"] / 1000)
            else:
                status, response_kind, response_body = 200, "native_nonstream_anthropic_message", nonstream_body

            response_path = f"response-{sequence:02}.body.raw"
            response_header_path = f"response-{sequence:02}.headers.raw"
            (run_root / response_path).write_bytes(response_body)

            if response_kind == "sse":
                status_line = b"HTTP/1.1 200 OK"
                raw_response_headers = send_headers(self, status_line, [
                    (b"Content-Type", b"text/event-stream"),
                    (b"Transfer-Encoding", b"chunked"),
                    (b"Connection", b"keep-alive"),
                ])
                (run_root / response_header_path).write_bytes(raw_response_headers)
                event_trace = []
                for index, (chunk, event) in enumerate(zip(chunks, sse_events), start=1):
                    delay_ms = event["delay_before_ms"]
                    time.sleep(delay_ms / 1000)
                    self.wfile.write(f"{len(chunk):X}\r\n".encode("ascii") + chunk + b"\r\n")
                    self.wfile.flush()
                    event_trace.append({
                        "index": index,
                        "event": event["event"],
                        "scheduled_delay_ms": delay_ms,
                        "chunk_bytes": len(chunk),
                        "chunk_sha256": sha256(chunk),
                    })
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
            else:
                status_text = b"404 Not Found" if status == 404 else b"200 OK"
                raw_response_headers = send_headers(self, b"HTTP/1.1 " + status_text, [
                    (b"Content-Type", b"application/json"),
                    (b"Content-Length", str(len(response_body)).encode("ascii")),
                    (b"Connection", b"keep-alive"),
                ])
                (run_root / response_header_path).write_bytes(raw_response_headers)
                self.wfile.write(response_body)
                self.wfile.flush()
                event_trace = []

            response_records.append({
                "sequence": sequence,
                "status": status,
                "kind": response_kind,
                "headers_path": response_header_path,
                "body_path": response_path,
                "body_bytes": len(response_body),
                "body_sha256": sha256(response_body),
                "event_trace": event_trace,
            })

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()

    env = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(dirs["home"]),
        "TMPDIR": str(dirs["tmp"]),
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "TZ": "UTC",
        "TERM": "dumb",
        "LINGXI_CONFIG_DIR": str(dirs["config"]),
        "LINGXI_API_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "LINGXI_NATIVE_ANTHROPIC_ENDPOINT": f"http://127.0.0.1:{server.server_port}",
        "ANTHROPIC_API_KEY": DUMMY_KEY,
        "ANTHROPIC_MAX_RETRIES": "0",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1",
        "DO_NOT_TRACK": "1",
    }
    args = [
        "--print", "--output-format", "json",
        "--setting-sources", "project", "--strict-mcp-config", "--tools", "",
        "--model", HARNESS_FIXTURE["model"],
        "--session-id", case["session_id"],
    ]
    if case["mode"] == "direct-nonstream":
        args.append("--no-stream")
    args.append(case["prompt"])
    command = [str(binary), *args]
    process_start_ns[0] = time.monotonic_ns()
    started = time.monotonic()
    timed_out = False
    try:
        completed = subprocess.run(
            command,
            cwd=dirs["cwd"],
            env=env,
            capture_output=True,
            timeout=HARNESS_FIXTURE["timeout_seconds"],
            check=False,
        )
        stdout, stderr, exit_code = completed.stdout, completed.stderr, completed.returncode
    except subprocess.TimeoutExpired as error:
        stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
    finally:
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=2)

    (run_root / "argv.raw.json").write_text(json.dumps(command, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    (run_root / "stdout.raw.json").write_bytes(stdout)
    (run_root / "stderr.raw").write_bytes(stderr)
    try:
        output = json.loads(stdout)
    except (UnicodeDecodeError, json.JSONDecodeError):
        output = None

    expected_timing_fields: list[str] | None
    if case["native_timing_reference"] == "timed_sse":
        expected_timing_fields = list(TIMED_RECEIPT["timing_fields_ms"].keys())
    elif case["native_timing_reference"] == "fallback_404":
        expected_timing_fields = list(FALLBACK_RECEIPT["native_timing_fields_ms"].keys())
    else:
        expected_timing_fields = None

    timing_names = [
        "time_to_request_ms", "ttft_stream_ms", "first_content_frame_ms",
        "ttft_ms", "first_output_token_ms", "duration_api_ms", "duration_ms",
    ]
    observed_timing = {
        name: output[name]
        for name in timing_names
        if isinstance(output, dict) and name in output
    }
    result_value = output.get("result") if isinstance(output, dict) else None
    stream_sequence = [request.get("stream") for request in request_records]
    checks = {
        "process_exited_zero": exit_code == 0 and not timed_out,
        "all_requests_loopback_dummy_key": bool(request_records) and all(
            request["accepted_loopback_dummy_key"] for request in request_records
        ),
        "request_stream_sequence": stream_sequence == case["expected_stream_sequence"],
        "fixed_result_content": result_value == case["expected_result"],
        "stdout_is_json_object": isinstance(output, dict),
        "native_timing_key_presence": (
            expected_timing_fields is None
            or all(name in output for name in expected_timing_fields)
            and all(name not in output for name in set(timing_names) - set(expected_timing_fields))
        ) if isinstance(output, dict) else expected_timing_fields is None,
        "observed_timing_values_finite_nonnegative": all(
            isinstance(value, (int, float)) and not isinstance(value, bool)
            and math.isfinite(value) and value >= 0
            for value in observed_timing.values()
        ),
    }
    for request in request_records:
        try:
            request_headers = (run_root / request["headers_path"]).read_bytes()
        except OSError:
            request_headers = b""
        try:
            parsed_headers = parse_headers(io.BytesIO(request_headers.split(b"\r\n", 1)[1]))
            request["parsed_header_count"] = len(parsed_headers.items())
        except (IndexError, ValueError, OSError):
            request["parsed_header_count"] = None

    receipt = {
        "product_binary": str(binary),
        "product_binary_sha256": sha256(binary.read_bytes()),
        "product_revision_argument": product_revision,
        "case": case,
        "command": command,
        "public_entrypoint": "Product CLI --print --output-format json; optional --no-stream control",
        "network_boundary": {
            "request_listener": "127.0.0.1 only",
            "dummy_api_key_only": True,
            "isolated_home_and_config": True,
            "upstream_requests": 0,
        },
        "dynamic_loopback_port": server.server_port,
        "timeout_seconds": HARNESS_FIXTURE["timeout_seconds"],
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "exit_code": exit_code,
        "timed_out": timed_out,
        "stdout": {"path": "stdout.raw.json", "bytes": len(stdout), "sha256": sha256(stdout)},
        "stderr": {"path": "stderr.raw", "bytes": len(stderr), "sha256": sha256(stderr)},
        "parsed_result": ({key: output.get(key) for key in (
            "type", "subtype", "is_error", "result", "num_turns", "stop_reason", "session_id"
        )} if isinstance(output, dict) else None),
        "native_timing_reference": case["native_timing_reference"],
        "expected_native_timing_key_presence": expected_timing_fields,
        "observed_timing_values_ms": observed_timing,
        "timing_comparison_policy": (
            "Assert key presence/absence and finite nonnegative values only; elapsed milliseconds are not byte-compared."
            if expected_timing_fields is not None
            else "Direct --no-stream has no separate Native capture; record timing values without claiming Native parity."
        ),
        "requests": request_records,
        "responses": response_records,
        "checks": checks,
        "artifacts_relative_to_receipt": [
            "argv.raw.json", "stdout.raw.json", "stderr.raw",
            *[request[path_key] for request in request_records for path_key in ("headers_path", "body_path")],
            *[response[path_key] for response in response_records for path_key in ("headers_path", "body_path")],
        ],
    }
    receipt_path = run_root / "receipt.json"
    receipt_path.write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return {"case_id": case["id"], "receipt": str(receipt_path.relative_to(ROOT)), "checks": checks}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path, help="Exact Product CLI executable to test")
    parser.add_argument("--product-revision", help="Checkpoint SHA represented by the supplied binary")
    parser.add_argument("--case", choices=["all", *[case["id"] for case in HARNESS_FIXTURE["cases"]]], default="all")
    args = parser.parse_args()
    binary = args.binary.expanduser().resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error(f"Product CLI binary is missing or not executable: {binary}")

    selected = HARNESS_FIXTURE["cases"] if args.case == "all" else [
        case for case in HARNESS_FIXTURE["cases"] if case["id"] == args.case
    ]
    reports = [run_case(case, binary, args.product_revision) for case in selected]
    print(json.dumps({"reports": reports}, ensure_ascii=False, indent=2))
    return 0 if all(all(item["checks"].values()) for item in reports) else 1


if __name__ == "__main__":
    raise SystemExit(main())
