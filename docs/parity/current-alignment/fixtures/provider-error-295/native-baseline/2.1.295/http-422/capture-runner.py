#!/usr/bin/env python3
"""One-shot Native 2.1.295 public PRINT HTTP 422 capture against localhost."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import threading
import time
from urllib.parse import urlsplit


HERE = Path(__file__).resolve().parent
INTEGRATION = HERE.parent.parent
CASE_ID = "native-print-http422-error-copy-295"
SESSION_ID = "00000000-0000-4000-8000-000000029508"
PROMPT = "Return the fixed Native HTTP 422 error-copy probe."
MODEL = "claude-sonnet-4-5"
API_KEY = "sk-ant-print-http422-local-dummy-only"
BINARY = Path("/Users/luolingfeng/.local/share/claude/versions/2.1.295")
BINARY_SHA256 = "0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70"
MAX_NATIVE_SECONDS = 60
PROCESS_RUN_SECONDS = 55
TERM_GRACE_SECONDS = 2
KILL_GRACE_SECONDS = 2
MAX_HEADER_BYTES = 128 * 1024
MAX_BODY_BYTES = 2 * 1024 * 1024
HTTP_422_BODY = (
    b'{"type":"error","error":{"type":"invalid_request_error",'
    b'"message":"NATIVE_295_PRINT_HTTP_422_ERROR_COPY_PROBE"}}'
)
OUTPUT_ROOT = INTEGRATION / "acceptance" / CASE_ID


class CaptureInterrupted(Exception):
    def __init__(self, signum: int):
        super().__init__(f"capture interrupted by signal {signum}")
        self.signum = signum


class RequestReadError(Exception):
    def __init__(self, message: str, raw: bytes):
        super().__init__(message)
        self.raw = raw


def sha256(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, ensure_ascii=True, indent=2) + "\n", encoding="ascii")


def load_manifest() -> tuple[dict, bytes, str]:
    manifest_path = HERE / "manifest.json"
    raw = manifest_path.read_bytes()
    manifest = json.loads(raw)
    runner = Path(__file__).resolve()
    relative = runner.relative_to(INTEGRATION).as_posix()
    if manifest.get("runner_relative_path") != relative:
        raise SystemExit("manifest runner path does not match this copy")
    if manifest.get("runner_sha256") != sha256(runner.read_bytes()):
        raise SystemExit("manifest runner SHA mismatch")
    expected = {
        "target_native_version": "2.1.295",
        "target_native_path": str(BINARY),
        "target_native_sha256": BINARY_SHA256,
        "capture_case_id": CASE_ID,
        "session_id": SESSION_ID,
        "output_root": OUTPUT_ROOT.relative_to(INTEGRATION).as_posix(),
        "mode": "public --bare --print --output-format json",
        "provider_route": "first-party Anthropic Messages to 127.0.0.1 only",
        "model": MODEL,
        "prompt": PROMPT,
        "http_status": 422,
        "expected_native_exit_code": 1,
        "native_process_launches": 1,
        "expected_messages_requests": 1,
        "max_native_seconds": MAX_NATIVE_SECONDS,
        "response_body_sha256": sha256(HTTP_422_BODY),
    }
    if any(manifest.get(key) != value for key, value in expected.items()):
        raise SystemExit("manifest capture contract mismatch")
    return manifest, raw, relative


def parse_request(connection: socket.socket) -> tuple[bytes, bytes, bytes, str, str, str, dict[str, list[str]]]:
    data = bytearray()
    separator = b"\r\n\r\n"
    while separator not in data:
        try:
            chunk = connection.recv(8192)
        except OSError as error:
            raise RequestReadError(f"header read failed: {type(error).__name__}: {error}", bytes(data)) from error
        if not chunk:
            raise RequestReadError("request ended before headers completed", bytes(data))
        data.extend(chunk)
        if separator not in data and len(data) > MAX_HEADER_BYTES:
            raise RequestReadError("request headers exceed limit", bytes(data))

    split_at = data.index(separator)
    header = bytes(data[:split_at + len(separator)])
    body = bytearray(data[split_at + len(separator):])
    try:
        lines = header[:-len(separator)].split(b"\r\n")
        method, target, protocol = lines[0].decode("latin1").split(" ", 2)
        parsed: dict[str, list[str]] = {}
        for line in lines[1:]:
            if b":" not in line:
                raise ValueError("malformed header field")
            name, value = line.split(b":", 1)
            parsed.setdefault(name.decode("latin1").lower(), []).append(value.lstrip().decode("latin1"))
        if parsed.get("transfer-encoding"):
            raise ValueError("transfer-encoded request framing is unsupported")
        lengths = parsed.get("content-length", [])
        if lengths and len(set(lengths)) != 1:
            raise ValueError("conflicting Content-Length headers")
        length = int(lengths[0]) if lengths else 0
        if length < 0 or length > MAX_BODY_BYTES:
            raise ValueError(f"request body length outside limit: {length}")
        if len(body) > length:
            raise ValueError("request contains bytes beyond Content-Length")
        while len(body) < length:
            try:
                chunk = connection.recv(min(65536, length - len(body)))
            except OSError as error:
                raise RequestReadError(
                    f"body read failed: {type(error).__name__}: {error}", header + bytes(body)
                ) from error
            if not chunk:
                raise ValueError("request body ended before Content-Length")
            body.extend(chunk)
        return header + bytes(body), header, bytes(body), method, target, protocol, parsed
    except (ValueError, UnicodeError) as error:
        raise RequestReadError(f"invalid request: {error}", header + bytes(body)) from error


def contains_prompt(value: object) -> bool:
    if isinstance(value, str):
        return PROMPT in value
    if isinstance(value, list):
        return any(contains_prompt(item) for item in value)
    if isinstance(value, dict):
        return any(contains_prompt(item) for item in value.values())
    return False


def main() -> int:
    if sys.argv != [sys.argv[0], "--print-http422"]:
        raise SystemExit("usage: capture_native_print_http422_295.py --print-http422")

    manifest, manifest_raw, runner_relative = load_manifest()
    if OUTPUT_ROOT.exists():
        raise SystemExit(f"Refusing one-shot capture because output path already exists: {OUTPUT_ROOT}")
    OUTPUT_ROOT.parent.mkdir(parents=True, exist_ok=True)
    # mkdir is the atomic one-shot claim; concurrent invocations cannot both own it.
    OUTPUT_ROOT.mkdir()
    case_dir = OUTPUT_ROOT / "attempt-01"
    case_dir.mkdir()
    runtime = case_dir / "runtime"
    runtime.mkdir()
    home = runtime / "home"
    config = runtime / "config"
    tmp = runtime / "tmp"
    cwd = runtime / "cwd"
    for directory in (home, config, tmp, cwd):
        directory.mkdir()

    initial = {
        "case": CASE_ID,
        "capture_mode": "public-print-http422",
        "provider": "firstParty",
        "model": MODEL,
        "session_id": SESSION_ID,
        "prompt": PROMPT,
        "phase": "preflight",
        "native_process_launches": 0,
        "one_shot_output_root": True,
        "runner_relative_path": runner_relative,
        "runner_sha256": sha256(Path(__file__).read_bytes()),
        "runner_manifest_sha256": sha256(manifest_raw),
        "native_sha256_expected": BINARY_SHA256,
        "native_sha256_actual": None,
        "timeout_budget_seconds": MAX_NATIVE_SECONDS,
        "client_retry_environment": {"ANTHROPIC_MAX_RETRIES": "0"},
        "request_semantics": "one Native process launch; Native client fallback/retry requests are captured separately and make this one-request capture incomplete",
    }
    write_json(case_dir / "case.json", initial)

    if not BINARY.is_file():
        initial["preflight_error"] = "pinned Native binary is missing"
        write_json(case_dir / "case.json", initial)
        return 3
    binary_hash = sha256(BINARY.read_bytes())
    initial["native_sha256_actual"] = binary_hash
    if binary_hash != BINARY_SHA256:
        initial["preflight_error"] = "pinned Native binary SHA mismatch"
        write_json(case_dir / "case.json", initial)
        return 3

    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        listener.bind(("127.0.0.1", 0))
        listener.listen(8)
        listener.settimeout(0.2)
    except OSError as error:
        listener.close()
        initial["preflight_error"] = f"loopback listener failed before Native launch: {type(error).__name__}: {error}"
        write_json(case_dir / "case.json", initial)
        return 4

    port = listener.getsockname()[1]
    base_url = f"http://127.0.0.1:{port}"
    sandbox = (
        "(version 1)\n"
        "(allow default)\n"
        "(deny network-outbound)\n"
        f'(allow network-outbound (remote ip "localhost:{port}"))\n'
        "(deny network-inbound)\n"
    ).encode("ascii")
    sandbox_path = case_dir / "native-child-network.sbpl"
    sandbox_path.write_bytes(sandbox)
    sandbox_check = subprocess.run(
        ["/usr/bin/sandbox-exec", "-f", str(sandbox_path), "/usr/bin/true"],
        cwd=cwd,
        env={"PATH": "/usr/bin:/bin", "HOME": str(home), "TMPDIR": str(tmp)},
        capture_output=True,
        timeout=8,
        check=False,
    )
    (case_dir / "sandbox-preflight.stdout.raw").write_bytes(sandbox_check.stdout)
    (case_dir / "sandbox-preflight.stderr.raw").write_bytes(sandbox_check.stderr)
    if sandbox_check.returncode != 0:
        listener.close()
        initial.update({
            "preflight_error": "sandbox profile preflight failed; Native was not launched",
            "sandbox_preflight_exit": sandbox_check.returncode,
        })
        write_json(case_dir / "case.json", initial)
        return 5

    env = {
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": str(home),
        "TMPDIR": str(tmp),
        "LANG": "C.UTF-8",
        "TZ": "UTC",
        "TERM": "xterm-256color",
        "CLAUDE_CONFIG_DIR": str(config),
        "ANTHROPIC_BASE_URL": base_url,
        "ANTHROPIC_API_KEY": API_KEY,
        "ANTHROPIC_MAX_RETRIES": "0",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1",
        "DO_NOT_TRACK": "1",
        "DISABLE_AUTOUPDATER": "1",
        "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "1",
    }
    argv = [
        str(BINARY), "--bare", "--print", "--output-format", "json",
        "--model", MODEL,
        "--system-prompt", "You are a local deterministic headless fixture.",
        "--tools", "",
        "--permission-mode", "dontAsk",
        "--permission-prompts", "none",
        "--session-id", SESSION_ID,
        PROMPT,
    ]
    write_json(case_dir / "invocation.json", {
        "argv": argv,
        "cwd": str(cwd),
        "env": {**env, "ANTHROPIC_API_KEY": "<fixed-dummy>"},
        "inherited_environment": False,
        "session_id": SESSION_ID,
        "prompt": PROMPT,
        "base_url": base_url,
        "native_process_launches_allowed": 1,
        "client_retry_environment": {"ANTHROPIC_MAX_RETRIES": "0"},
    })

    records: list[dict] = []
    server_errors: list[str] = []
    done = threading.Event()
    active_lock = threading.Lock()
    active_connections: set[socket.socket] = set()

    def serve() -> None:
        while not done.is_set():
            try:
                connection, address = listener.accept()
            except socket.timeout:
                continue
            except OSError as error:
                if not done.is_set():
                    server_errors.append(f"accept: {type(error).__name__}: {error}")
                return
            with active_lock:
                active_connections.add(connection)
            try:
                with connection:
                    connection.settimeout(2)
                    index = len(records)
                    record: dict = {"index": index, "peer": address[0]}
                    try:
                        raw_request, header_raw, body_raw, method, target, protocol, headers = parse_request(connection)
                    except RequestReadError as error:
                        raw_request = error.raw
                        header_raw = raw_request.split(b"\r\n\r\n", 1)[0] + b"\r\n\r\n" if b"\r\n\r\n" in raw_request else raw_request
                        body_raw = raw_request[len(header_raw):] if raw_request.startswith(header_raw) else b""
                        request_prefix = case_dir / f"request-{index:03d}"
                        request_prefix.with_suffix(".raw").write_bytes(raw_request)
                        (case_dir / f"request-{index:03d}-headers.raw").write_bytes(header_raw)
                        (case_dir / f"request-{index:03d}-body.raw").write_bytes(body_raw)
                        record.update({
                            "parse_error": str(error),
                            "messages_request": False,
                            "raw_request_path": f"request-{index:03d}.raw",
                            "raw_header_path": f"request-{index:03d}-headers.raw",
                            "raw_body_path": f"request-{index:03d}-body.raw",
                        })
                        records.append(record)
                        server_errors.append(f"request {index}: {error}")
                        response = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                        status = 400
                    else:
                        request_prefix = case_dir / f"request-{index:03d}"
                        request_prefix.with_suffix(".raw").write_bytes(raw_request)
                        (case_dir / f"request-{index:03d}-headers.raw").write_bytes(header_raw)
                        (case_dir / f"request-{index:03d}-body.raw").write_bytes(body_raw)
                        try:
                            body_json = json.loads(body_raw) if body_raw else None
                        except (UnicodeDecodeError, json.JSONDecodeError):
                            body_json = None
                        path = urlsplit(target).path
                        is_messages = method == "POST" and path == "/v1/messages"
                        header_map = {key.lower(): value for key, value in headers.items()}
                        record.update({
                            "method": method,
                            "target": target,
                            "protocol": protocol,
                            "request_bytes": len(raw_request),
                            "request_sha256": sha256(raw_request),
                            "headers_sha256": sha256(header_raw),
                            "body_sha256": sha256(body_raw),
                            "messages_request": is_messages,
                            "health_check": method == "HEAD" and path == "/api/hello",
                            "model": body_json.get("model") if isinstance(body_json, dict) else None,
                            "body_json_valid": isinstance(body_json, dict),
                            "prompt_matches_fixed_input": contains_prompt(body_json),
                            "x_api_key_is_dummy": API_KEY in header_map.get("x-api-key", []),
                            "raw_request_path": f"request-{index:03d}.raw",
                            "raw_header_path": f"request-{index:03d}-headers.raw",
                            "raw_body_path": f"request-{index:03d}-body.raw",
                        })
                        records.append(record)
                        if record["health_check"]:
                            response = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            status = 200
                        elif is_messages and isinstance(body_json, dict):
                            response = (
                                f"HTTP/1.1 422 Unprocessable Entity\r\nContent-Type: application/json\r\nContent-Length: {len(HTTP_422_BODY)}\r\nConnection: close\r\n\r\n".encode("ascii")
                                + HTTP_422_BODY
                            )
                            status = 422
                        else:
                            response = b"HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                            status = 404

                    response_body = response.split(b"\r\n\r\n", 1)[1]
                    (case_dir / f"response-{index:03d}.raw").write_bytes(response)
                    (case_dir / f"response-{index:03d}-body.raw").write_bytes(response_body)
                    record["status"] = status
                    record["response_bytes"] = len(response)
                    record["response_sha256"] = sha256(response)
                    record["response_body_sha256"] = sha256(response_body)
                    try:
                        connection.sendall(response)
                    except OSError as error:
                        record["response_send_error"] = f"{type(error).__name__}: {error}"
            except Exception as error:
                server_errors.append(f"connection: {type(error).__name__}: {error}")
            finally:
                with active_lock:
                    active_connections.discard(connection)

    server_thread = threading.Thread(target=serve, name="http422-local-stub", daemon=True)
    server_thread.start()

    old_handlers = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
    cleanup_started = False

    def interrupt(signum: int, _frame: object) -> None:
        if cleanup_started:
            return
        raise CaptureInterrupted(signum)

    process: subprocess.Popen[bytes] | None = None
    stdout = b""
    stderr = b""
    timed_out = False
    interrupted_signal: int | None = None
    launch_error: str | None = None
    started_at: float | None = None
    try:
        signal.signal(signal.SIGINT, interrupt)
        signal.signal(signal.SIGTERM, interrupt)
        initial.update({
            "phase": "native-starting",
            "sandbox_profile_sha256": sha256(sandbox),
            "sandbox_preflight_exit": sandbox_check.returncode,
            "loopback_port": port,
        })
        write_json(case_dir / "case.json", initial)
        started_at = time.monotonic()
        previous_signal_mask = signal.pthread_sigmask(
            signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM}
        )
        try:
            process = subprocess.Popen(
                ["/usr/bin/sandbox-exec", "-f", str(sandbox_path), *argv],
                cwd=cwd,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                start_new_session=True,
            )
        except OSError as error:
            launch_error = f"{type(error).__name__}: {error}"
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, previous_signal_mask)
        if process is not None:
            initial["native_process_launches"] = 1
            initial["native_launched"] = True
            initial["phase"] = "native-running"
            write_json(case_dir / "case.json", initial)
            try:
                stdout, stderr = process.communicate(timeout=PROCESS_RUN_SECONDS)
            except subprocess.TimeoutExpired:
                timed_out = True
            except CaptureInterrupted as error:
                interrupted_signal = error.signum
    except CaptureInterrupted as error:
        interrupted_signal = error.signum
    finally:
        # Keep the handlers active but non-raising during teardown and artifact writes.
        cleanup_started = True
        if process is not None:
            if process.poll() is None:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    stdout, stderr = process.communicate(timeout=TERM_GRACE_SECONDS)
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    try:
                        stdout, stderr = process.communicate(timeout=KILL_GRACE_SECONDS)
                    except subprocess.TimeoutExpired as error:
                        stdout = error.output or b""
                        stderr = error.stderr or b""
                        server_errors.append("Native process group remained after SIGKILL grace period")
            elif process.poll() is not None:
                try:
                    stdout, stderr = process.communicate(timeout=TERM_GRACE_SECONDS)
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    try:
                        stdout, stderr = process.communicate(timeout=KILL_GRACE_SECONDS)
                    except subprocess.TimeoutExpired as error:
                        stdout = error.output or b""
                        stderr = error.stderr or b""
                        server_errors.append("Native output pipes remained open after SIGKILL cleanup")
        done.set()
        listener.close()
        with active_lock:
            active = list(active_connections)
        for connection in active:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                connection.close()
            except OSError:
                pass
        server_thread.join(timeout=3)

    exit_code = process.returncode if process is not None else None
    (case_dir / "native.stdout.raw").write_bytes(stdout)
    (case_dir / "native.stderr.raw").write_bytes(stderr)
    write_json(case_dir / "native-exit.json", {
        "returncode": exit_code,
        "launch_error": launch_error,
        "timed_out": timed_out,
        "interrupted_signal": interrupted_signal,
        "native_process_launches": 1 if process is not None else 0,
        "elapsed_seconds": round(time.monotonic() - started_at, 3) if started_at is not None else 0,
        "max_native_seconds": MAX_NATIVE_SECONDS,
        "stdout_bytes": len(stdout),
        "stdout_sha256": sha256(stdout),
        "stderr_bytes": len(stderr),
        "stderr_sha256": sha256(stderr),
    })
    write_json(case_dir / "requests.json", records)
    write_json(case_dir / "server-errors.json", server_errors)

    messages = [record for record in records if record.get("messages_request")]
    all_requests_expected = all(record.get("messages_request") or record.get("health_check") for record in records)
    request_route_ok = all(
        record.get("peer") == "127.0.0.1"
        and urlsplit(record.get("target", "")).path == "/v1/messages"
        for record in messages
    )
    request_identity_ok = all(
        record.get("model") == MODEL
        and record.get("body_json_valid") is True
        and record.get("prompt_matches_fixed_input") is True
        and record.get("x_api_key_is_dummy") is True
        for record in messages
    )
    response_status_ok = len(messages) == 1 and messages[0].get("status") == 422
    response_delivery_ok = len(messages) == 1 and not messages[0].get("response_send_error")
    process_exit_ok = process is not None and exit_code == 1 and not timed_out and interrupted_signal is None
    server_stopped = not server_thread.is_alive()
    capture_complete = (
        process_exit_ok
        and len(messages) == 1
        and all_requests_expected
        and request_route_ok
        and request_identity_ok
        and response_status_ok
        and response_delivery_ok
        and not server_errors
        and server_stopped
    )
    result = {
        **initial,
        "phase": "complete",
        "native_exit_code": exit_code,
        "expected_native_exit_code": 1,
        "native_exit_code_matches_expected": exit_code == 1,
        "native_process_launches": 1 if process is not None else 0,
        "messages_request_count": len(messages),
        "all_request_count": len(records),
        "all_requests_are_expected": all_requests_expected,
        "request_route_is_loopback_messages": request_route_ok,
        "request_model_prompt_and_dummy_key_match": request_identity_ok,
        "http_422_observed_on_single_message_request": response_status_ok,
        "response_delivery_valid": response_delivery_ok,
        "timed_out": timed_out,
        "interrupted_signal": interrupted_signal,
        "server_thread_stopped": server_stopped,
        "server_errors": server_errors,
        "capture_complete": capture_complete,
        "capture_scope": "one Native process launch and exactly one Messages request; extra client fallback/retry requests are preserved and make capture incomplete",
        "parity_verdict": "not assessed; this is one captured tuple only",
    }
    write_json(case_dir / "case.json", result)
    for sig, handler in old_handlers.items():
        signal.signal(sig, handler)
    print(json.dumps({key: result[key] for key in (
        "case", "native_process_launches", "native_exit_code", "timed_out",
        "interrupted_signal", "messages_request_count", "all_request_count", "capture_complete",
    )}, indent=2))
    return 0 if capture_complete else 1


if __name__ == "__main__":
    raise SystemExit(main())
