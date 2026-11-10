#!/usr/bin/env python3
"""Run the complete macOS MCP-login CLI against a fake local OAuth service.

The sandbox denies all Mach lookups, host credential files and security/open
execution. A fake canary proves that isolation before the native CLI runs.
Existing HOME is preserved; CLAUDE_CONFIG_DIR supplies the disposable store.
"""
import argparse
import base64
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import pty
import re
import signal
import socket
import subprocess
import threading
import time
from urllib.parse import parse_qs, urlencode, urlsplit

SHA = "a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--callback-extra-keys", type=int, default=0)
    parser.add_argument("--cancel", action="store_true")
    args = parser.parse_args()
    assert hashlib.sha256(args.binary.read_bytes()).hexdigest() == SHA
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    for name in ["config", "cwd", "bin", "tmp"]:
        (root / name).mkdir()
    canary = root / "credential-canary"
    canary.write_text("fake-isolation-canary")
    forbidden = [str(canary), "/Users/luolingfeng/.claude", "/Users/luolingfeng/.claude.json",
                 "/Users/luolingfeng/.config/claude", "/Users/luolingfeng/Library/Keychains",
                 "/Library/Keychains", "/private/var/Keychains"]
    profile = "(version 1)\n(allow default)\n(deny mach-lookup)\n"
    profile += '(deny process-exec (literal "/usr/bin/security") (literal "/usr/bin/open"))\n'
    profile += "(deny file-read* file-write* " + " ".join("(subpath " + json.dumps(p) + ")" for p in forbidden) + ")\n"
    (root / "isolation.sb").write_text(profile)
    for name in ["security", "open"]:
        shim = root / "bin" / name
        shim.write_text("#!/bin/sh\nexit 44\n")
        shim.chmod(0o755)
    env = {k: v for k, v in os.environ.items() if k in ["HOME", "PATH", "TERM", "LANG", "LC_ALL"]}
    env.update({"PATH": str(root / "bin") + ":/usr/bin:/bin:/usr/sbin:/sbin", "TMPDIR": str(root / "tmp"),
                "CLAUDE_CONFIG_DIR": str(root / "config"), "ANTHROPIC_API_KEY": "sk-ant-offline-cli-fixture",
                "ANTHROPIC_BASE_URL": "http://127.0.0.1:9", "HTTP_PROXY": "http://127.0.0.1:9",
                "HTTPS_PROXY": "http://127.0.0.1:9", "ALL_PROXY": "http://127.0.0.1:9",
                "NO_PROXY": "127.0.0.1,localhost", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
                "DISABLE_AUTOUPDATER": "1", "DISABLE_TELEMETRY": "1", "DISABLE_ERROR_REPORTING": "1"})
    prefix = ["/usr/bin/sandbox-exec", "-f", str(root / "isolation.sb")]
    proof = []
    for label, command in [("canary", ["/bin/cat", str(canary)]), ("security-exec", ["/usr/bin/security", "-h"]),
                           ("native-version", [str(args.binary), "--version"])]:
        result = subprocess.run(prefix + command, cwd=root / "cwd", env=env, capture_output=True, timeout=20)
        proof.append({"label": label, "exitCode": result.returncode,
                      "stdout": result.stdout.decode(errors="replace"), "stderr": result.stderr.decode(errors="replace")})
    (root / "isolation-proof.json").write_text(json.dumps(proof, indent=2))
    assert proof[0]["exitCode"] != 0 and "Operation not permitted" in proof[0]["stderr"]
    assert proof[1]["exitCode"] != 0 and "Operation not permitted" in proof[1]["stderr"]
    assert proof[2]["exitCode"] == 0 and "2.1.322" in proof[2]["stdout"]
    traffic, authorization = [], {}
    lock = threading.Lock()

    class Peer(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def record(self):
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            with lock:
                traffic.append({"method": self.command, "path": self.path,
                                "headers": list(self.headers.items()), "body": raw.decode()})
                (root / "traffic.json").write_text(json.dumps(traffic, indent=2))
            return raw

        def reply(self, status, body=None, **headers):
            raw = b"" if body is None else json.dumps(body, separators=(",", ":")).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            for name, value in headers.items():
                self.send_header(name.replace("_", "-"), value)
            self.end_headers()
            self.wfile.write(raw)

        def do_GET(self):
            self.record()
            parsed = urlsplit(self.path)
            if parsed.path.startswith("/.well-known/oauth-protected-resource"):
                return self.reply(200, {"resource": base + "/mcp", "authorization_servers": [base], "scopes_supported": ["read", "write"]})
            if parsed.path == "/.well-known/oauth-authorization-server":
                return self.reply(200, {"issuer": base, "authorization_endpoint": base + "/authorize",
                                       "token_endpoint": base + "/token", "registration_endpoint": base + "/register",
                                       "response_types_supported": ["code"], "grant_types_supported": ["authorization_code", "refresh_token"],
                                       "code_challenge_methods_supported": ["S256"], "token_endpoint_auth_methods_supported": ["none"],
                                       "scopes_supported": ["read", "write", "offline_access"], "authorization_response_iss_parameter_supported": True})
            if parsed.path == "/authorize":
                authorization.update(parse_qs(parsed.query))
                redirect = authorization["redirect_uri"][0] + "?" + urlencode({"code": "fake +~!*code", "state": authorization["state"][0], "iss": base})
                return self.reply(302, Location=redirect)
            if parsed.path == "/mcp":
                return self.reply(401, {"error": "unauthorized"}, WWW_Authenticate='Bearer resource_metadata="' + base + '/.well-known/oauth-protected-resource"')
            self.reply(404)

        def do_POST(self):
            raw = self.record()
            if self.path == "/register":
                body = json.loads(raw)
                return self.reply(201, {**body, "client_id": "fake-cli-dcr-client"})
            if self.path == "/token":
                form = parse_qs(raw.decode(), keep_blank_values=True)
                assert form["client_id"] == ["fake-cli-dcr-client"]
                assert form["grant_type"] == ["authorization_code"]
                assert form["code"] == ["fake +~!*code"]
                challenge = base64.urlsafe_b64encode(hashlib.sha256(form["code_verifier"][0].encode()).digest()).decode().rstrip("=")
                assert authorization["code_challenge"] == [challenge]
                assert form["redirect_uri"] == authorization["redirect_uri"]
                return self.reply(200, {"access_token": "fake-cli-access", "refresh_token": "fake-cli-refresh",
                                       "token_type": "Bearer", "expires_in": 3600, "scope": "read write offline_access"})
            if self.path == "/mcp":
                if self.headers.get("Authorization") != "Bearer fake-cli-access":
                    return self.reply(401, {"error": "unauthorized"}, WWW_Authenticate='Bearer resource_metadata="' + base + '/.well-known/oauth-protected-resource"')
                frame = json.loads(raw)
                if "id" not in frame:
                    return self.reply(202)
                if frame["method"] == "initialize":
                    result = {"protocolVersion": frame["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "fake-oauth", "version": "1"}}
                elif frame["method"] == "tools/list":
                    result = {"tools": []}
                else:
                    result = {}
                return self.reply(200, {"jsonrpc": "2.0", "id": frame["id"], "result": result})
            self.reply(404)

    peer = ThreadingHTTPServer(("127.0.0.1", 0), Peer)
    peer.daemon_threads = True
    base = "http://127.0.0.1:" + str(peer.server_port)
    threading.Thread(target=peer.serve_forever, daemon=True).start()
    process = None
    try:
        added = subprocess.run(prefix + [str(args.binary), "mcp", "add", "--transport", "http", "--scope", "local", "fake-oauth", base + "/mcp"],
                               cwd=root / "cwd", env=env, capture_output=True, timeout=25)
        (root / "add.stdout").write_bytes(added.stdout)
        (root / "add.stderr").write_bytes(added.stderr)
        assert added.returncode == 0, added.stderr.decode(errors="replace")[-1000:]
        master, slave = pty.openpty()
        process = subprocess.Popen(prefix + [str(args.binary), "mcp", "login", "fake-oauth", "--no-browser"], cwd=root / "cwd", env=env,
                                   stdin=slave, stdout=slave, stderr=subprocess.PIPE)
        os.close(slave)
        terminal = os.fdopen(master, "rb", buffering=0)
        chunks = {"stdout": bytearray(), "stderr": bytearray()}
        def read(stream, name):
            with (root / ("login." + name)).open("wb") as out:
                while True:
                    try:
                        raw = (stream.read1 if hasattr(stream, "read1") else stream.read)(4096)
                    except OSError:
                        break
                    if not raw:
                        break
                    out.write(raw); out.flush()
                    with lock:
                        chunks[name].extend(raw)
        for name in chunks:
            threading.Thread(target=read, args=(terminal if name == "stdout" else process.stderr, name), daemon=True).start()
        deadline = time.monotonic() + 40
        callback = None
        callback_address = None
        while time.monotonic() < deadline and process.poll() is None:
            with lock:
                text = bytes(chunks["stdout"] + chunks["stderr"]).decode(errors="replace")
            match = re.search(re.escape(base) + r"/authorize\?[^\x00-\x20\x7f]+", text)
            if match and callback is None:
                parsed = urlsplit(match.group())
                redirect_uri = urlsplit(parse_qs(parsed.query)["redirect_uri"][0])
                callback_address = (redirect_uri.hostname, redirect_uri.port)
                if args.cancel:
                    callback = "cancelled"
                    process.send_signal(signal.SIGINT)
                    continue
                connection = http.client.HTTPConnection("127.0.0.1", peer.server_port, timeout=5)
                connection.request("GET", parsed.path + "?" + parsed.query)
                response = connection.getresponse()
                assert response.status == 302
                callback = response.getheader("Location")
                response.read(); connection.close()
                if args.callback_extra_keys:
                    parsed_callback = urlsplit(callback)
                    callback_connection = http.client.HTTPConnection(parsed_callback.hostname, parsed_callback.port, timeout=2)
                    callback_query = "&".join(["x=1"] * args.callback_extra_keys) + "&" + parsed_callback.query
                    try:
                        callback_connection.request("GET", parsed_callback.path + "?" + callback_query)
                        callback_response = callback_connection.getresponse()
                        (root / "callback-response.json").write_text(json.dumps({"extraKeys": args.callback_extra_keys,
                            "status": callback_response.status, "body": callback_response.read().decode(errors="replace")}))
                    except Exception as error:
                        (root / "callback-response.json").write_text(json.dumps({"extraKeys": args.callback_extra_keys, "error": str(error)}))
                    finally:
                        callback_connection.close()
                else:
                    os.write(master, (callback + "\n").encode())
            time.sleep(.05)
        exit_code = process.wait(timeout=3)
        report = {"nativeSha256": SHA, "scope": "complete native mcp login, fake local service and isolated credential store",
                  "isolationProved": True, "exitCode": exit_code, "authorization": authorization,
                  "trafficCount": len(traffic), "paths": [row["path"] for row in traffic],
                  "callbackExtraKeys": args.callback_extra_keys,
                  "cancelled": args.cancel,
                  "tokenExchanged": any(row["path"] == "/token" for row in traffic)}
        if args.cancel:
            try:
                with socket.create_connection(callback_address, timeout=1):
                    report["callbackListenerClosed"] = False
            except OSError:
                report["callbackListenerClosed"] = True
        (root / "report.json").write_text(json.dumps(report, indent=2))
        if args.cancel:
            assert not report["tokenExchanged"] and report["callbackListenerClosed"], report
        else:
            assert exit_code == 0 and report["tokenExchanged"], report
        print(json.dumps({"passed": True, "requests": len(traffic), "report": str(root / "report.json")}))
    finally:
        if process and process.poll() is None:
            process.kill(); process.wait()
        peer.shutdown(); peer.server_close()


if __name__ == "__main__":
    main()
