#!/usr/bin/env python3
"""SHA-pinned native MCP TLS/proxy probes; every endpoint is loopback only.

--serve exposes the same peers to Rust candidate tests without starting Claude.
The certificates and proxy credentials are disposable test material.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import queue
import select
import shutil
import signal
import socket
import ssl
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

from compact_live_oracle import isolated_env
from mcp_hooks_322_live import SHA256, blocks, make_handler as make_api_handler

VERSION = "2.1.322"
GOOD = "WIRE_NETWORK_322_OK"
SCENARIOS = (
    "tls-untrusted", "tls-extra-ca", "tls-ssl-cert-file", "tls-ssl-cert-dir", "tls-wrong-host",
    "tls-mtls-missing", "tls-mtls", "tls-mtls-encrypted", "tls-mtls-wrong-passphrase", "tls-mtls-wrong-pair",
    "proxy-forward", "proxy-basic", "proxy-bypass", "proxy-port-mismatch",
    "proxy-connect", "proxy-connect-basic", "sse-tls-extra-ca", "sse-proxy-forward",
)
ENV_KEYS = (
    "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy",
    "NO_PROXY", "no_proxy", "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS",
    "NODE_TLS_REJECT_UNAUTHORIZED", "CLAUDE_CODE_CLIENT_CERT", "CLAUDE_CODE_CLIENT_KEY",
    "CLAUDE_CODE_CLIENT_KEY_PASSPHRASE", "LINGXI_CLIENT_CERT", "LINGXI_CLIENT_KEY",
    "LINGXI_CLIENT_KEY_PASSPHRASE", "CLAUDE_CODE_CERT_STORE", "LINGXI_CERT_STORE",
)


def encoded(value):
    return json.dumps(value, ensure_ascii=True, separators=(",", ":")).encode()


def dump(path, value):
    Path(path).write_text(json.dumps(value, ensure_ascii=True, indent=2) + "\n")


def certificates(directory):
    directory.mkdir(parents=True, exist_ok=True)
    if (directory / "client-encrypted-key.pem").exists():
        return
    openssl = shutil.which("openssl") or "/usr/bin/openssl"
    def run(*args):
        subprocess.run([openssl, *args], cwd=directory, check=True, capture_output=True)
    run("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca-key.pem", "-out", "ca.pem",
        "-days", "3650", "-subj", "/CN=MCP local network 322 test CA",
        "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    for name, san, usage in (
        ("server", "DNS:localhost,IP:127.0.0.1", "serverAuth"),
        ("wrong", "DNS:wrong.invalid", "serverAuth"),
        ("client", "DNS:mcp-test-client", "clientAuth"),
    ):
        run("req", "-new", "-newkey", "rsa:2048", "-nodes", "-keyout", f"{name}-key.pem",
            "-out", f"{name}.csr", "-subj", f"/CN={name}-mcp-network-322")
        (directory / f"{name}.ext").write_text(
            f"basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage={usage}\nsubjectAltName={san}\n")
        run("x509", "-req", "-in", f"{name}.csr", "-CA", "ca.pem", "-CAkey", "ca-key.pem",
            "-CAcreateserial", "-out", f"{name}-cert.pem", "-days", "3650", "-extfile", f"{name}.ext")
    run("pkcs8", "-topk8", "-in", "client-key.pem", "-out", "client-encrypted-key.pem",
        "-v2", "aes-256-cbc", "-passout", "pass:loopback-fixture-only")


class CaptureReader:
    def __init__(self, source):
        self.source, self.lines = source, []
    def readline(self, *args):
        line = self.source.readline(*args)
        self.lines.append(line)
        return line
    def __getattr__(self, name):
        return getattr(self.source, name)


class RawHeaders:
    def parse_request(self):
        source = self.rfile
        captured = CaptureReader(source)
        self.rfile = captured
        try:
            valid = super().parse_request()
            self.raw_header_bytes = b"".join(captured.lines)
            return valid
        finally:
            self.rfile = source
    def log_message(self, *_args):
        pass


class Server(ThreadingHTTPServer):
    daemon_threads = True


class Peers:
    def __init__(self, root, scenario, certs):
        self.root, self.scenario = root, scenario
        self.stop, self.lock = threading.Event(), threading.Lock()
        self.traffic, self.proxy_traffic = [], []
        self.servers, self.threads = [], []
        self.legacy = scenario.startswith("sse-")
        self.tls = scenario.startswith("tls-") or "tls-" in scenario or "connect" in scenario
        self.proxy = scenario.startswith("proxy-") or scenario == "sse-proxy-forward"
        self.expected_success = scenario not in ("tls-untrusted", "tls-ssl-cert-file", "tls-ssl-cert-dir", "tls-wrong-host",
                                                 "tls-mtls-missing", "tls-mtls-wrong-passphrase", "tls-mtls-wrong-pair")
        owner = self
        replies = queue.Queue()
        class Peer(RawHeaders, BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"
            def record(self, raw):
                with owner.lock:
                    index = len(owner.traffic) + 1
                    wire = self.raw_requestline + self.raw_header_bytes + raw
                    path = root / f"peer-{index:02d}-{self.command}.http"
                    path.write_bytes(wire)
                    peer_cert = self.connection.getpeercert() if isinstance(self.connection, ssl.SSLSocket) else None
                    body = json.loads(raw) if raw else None
                    row = {"method":self.command,"path":self.path,"headers":list(self.headers.items()),
                           "wire_path":str(path),"wire_sha256":hashlib.sha256(wire).hexdigest(),
                           "body":body,"tls_client_certificate":peer_cert}
                    owner.traffic.append(row)
                    dump(root / "peer-traffic.json", owner.traffic)
                return body
            def reply(self, status, body=b"", content_type=None):
                self.send_response(status)
                if content_type:
                    self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True
                try:
                    self.wfile.write(body)
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass
            def do_GET(self):
                self.record(b"")
                if not owner.legacy:
                    return self.reply(405)
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                try:
                    self.wfile.write(b"event: endpoint\ndata: /messages\n\n")
                    self.wfile.flush()
                    while not owner.stop.is_set():
                        try:
                            response = replies.get(timeout=.1)
                        except queue.Empty:
                            continue
                        self.wfile.write(b"data: " + encoded(response) + b"\n\n")
                        self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                self.close_connection = True
            def do_DELETE(self):
                self.record(b"")
                self.reply(200)
            def do_POST(self):
                raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                body = self.record(raw)
                if body.get("id") is None:
                    return self.reply(202)
                response = {"jsonrpc":"2.0","id":body["id"]}
                method = body.get("method")
                if method == "server/discover":
                    response["error"] = {"code":-32601,"message":"Method not found"}
                elif method == "initialize":
                    response["result"] = {"protocolVersion":body["params"]["protocolVersion"],
                        "capabilities":{"tools":{}},"serverInfo":{"name":"network322","version":"1"}}
                elif method == "tools/list":
                    response["result"] = {"tools":[{"name":"echo","description":"Local network probe",
                        "inputSchema":{"type":"object","properties":{"value":{"type":"string"}}},
                        "annotations":{"readOnlyHint":True}}]}
                elif method == "tools/call":
                    response["result"] = {"content":[{"type":"text","text":GOOD}]}
                elif method == "ping":
                    response["result"] = {"echo":GOOD}
                else:
                    response["result"] = {}
                if owner.legacy:
                    replies.put(response)
                    return self.reply(202)
                self.reply(200, encoded(response), "application/json")
        peer = Server(("127.0.0.1", 0), Peer)
        if self.tls:
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            name = "wrong" if scenario == "tls-wrong-host" else "server"
            context.load_cert_chain(certs / f"{name}-cert.pem", certs / f"{name}-key.pem")
            if "mtls" in scenario:
                context.verify_mode = ssl.CERT_REQUIRED
                context.load_verify_locations(certs / "ca.pem")
            peer.socket = context.wrap_socket(peer.socket, server_side=True)
        self.peer = peer
        self.servers.append(peer)
        self.env = {}
        self.url = f"{'https' if self.tls else 'http'}://127.0.0.1:{peer.server_port}/mcp"
        if scenario == "tls-ssl-cert-dir":
            self.env["SSL_CERT_DIR"] = str(certs)
        elif self.tls and scenario != "tls-untrusted":
            self.env["SSL_CERT_FILE" if scenario == "tls-ssl-cert-file" else "NODE_EXTRA_CA_CERTS"] = str(certs / "ca.pem")
        if "mtls" in scenario and scenario != "tls-mtls-missing":
            encrypted = scenario.endswith("-encrypted") or scenario == "tls-mtls-wrong-passphrase"
            key_name = "server-key.pem" if scenario == "tls-mtls-wrong-pair" else "client-encrypted-key.pem" if encrypted else "client-key.pem"
            self.env.update({"CLAUDE_CODE_CLIENT_CERT":str(certs / "client-cert.pem"),
                             "CLAUDE_CODE_CLIENT_KEY":str(certs / key_name)})
            if encrypted:
                self.env["CLAUDE_CODE_CLIENT_KEY_PASSPHRASE"] = "wrong-fixture-passphrase" if scenario == "tls-mtls-wrong-passphrase" else "loopback-fixture-only"
        if self.proxy:
            self.proxy_server = self.make_proxy()
            self.servers.append(self.proxy_server)
            credentials = "probe:pass@" if scenario.endswith("-basic") else ""
            self.env.update({"HTTPS_PROXY":f"http://{credentials}127.0.0.1:{self.proxy_server.server_port}",
                             "NO_PROXY":"localhost"})
            if scenario == "proxy-bypass":
                self.env["NO_PROXY"] = "localhost,127.0.0.1"
            elif scenario == "proxy-port-mismatch":
                self.env["NO_PROXY"] = "localhost,127.0.0.1:1"
        for server in self.servers:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            self.threads.append(thread)

    def make_proxy(self):
        owner = self
        class Proxy(RawHeaders, BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"
            def record(self, raw):
                with owner.lock:
                    index = len(owner.proxy_traffic) + 1
                    wire = self.raw_requestline + self.raw_header_bytes + raw
                    path = owner.root / f"proxy-{index:02d}-{self.command}.http"
                    path.write_bytes(wire)
                    owner.proxy_traffic.append({"method":self.command,"target":self.path,
                        "headers":list(self.headers.items()),"wire_path":str(path),
                        "wire_sha256":hashlib.sha256(wire).hexdigest()})
                    dump(owner.root / "proxy-traffic.json", owner.proxy_traffic)
            def relay(self, upstream):
                self.connection.setblocking(False)
                upstream.setblocking(False)
                try:
                    while not owner.stop.is_set():
                        readable, _, _ = select.select([self.connection, upstream], [], [], .1)
                        for source in readable:
                            data = source.recv(65536)
                            if not data:
                                return
                            (upstream if source is self.connection else self.connection).sendall(data)
                except (BrokenPipeError, ConnectionResetError, OSError):
                    pass
                finally:
                    upstream.close()
                    self.close_connection = True
            def connect_target(self, host, port):
                if host not in ("127.0.0.1", "localhost") or port != owner.peer.server_port:
                    self.send_error(403, "Only the owned loopback peer is allowed")
                    return None
                return socket.create_connection((host, port), timeout=3)
            def do_CONNECT(self):
                self.record(b"")
                host, port = self.path.rsplit(":", 1)
                upstream = self.connect_target(host, int(port))
                if upstream is None:
                    return
                self.wfile.write(b"HTTP/1.1 200 Connection established\r\n\r\n")
                self.wfile.flush()
                self.relay(upstream)
            def forward(self):
                raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                self.record(raw)
                target = urlsplit(self.path)
                upstream = self.connect_target(target.hostname, target.port or 80)
                if upstream is None:
                    return
                path = target.path or "/"
                if target.query:
                    path += "?" + target.query
                wire = f"{self.command} {path} HTTP/1.1\r\n".encode()
                for name, value in self.headers.items():
                    if name.lower() not in ("proxy-authorization", "proxy-connection"):
                        wire += f"{name}: {value}\r\n".encode("latin1")
                upstream.sendall(wire + b"\r\n" + raw)
                self.relay(upstream)
            do_GET = forward
            do_POST = forward
            do_DELETE = forward
        return Server(("127.0.0.1", 0), Proxy)

    def config(self):
        return {"url":self.url,"transport":"sse" if self.legacy else "http",
                "env":self.env,"clear_env":ENV_KEYS,"expected_success":self.expected_success}

    def close(self):
        self.stop.set()
        for server in self.servers:
            server.shutdown()
            server.server_close()
        for thread in self.threads:
            thread.join(timeout=2)

    def report(self):
        return {**self.config(),"scenario":self.scenario,"peer_requests":self.traffic,
                "proxy_requests":self.proxy_traffic}


def run_native(binary, root, scenario, certs, timeout):
    root.mkdir(parents=True, exist_ok=False)
    for name in ("config", "work", "tmp"):
        (root / name).mkdir()
    (root / "config/.claude.json").write_text('{"hasCompletedOnboarding":true}')
    requests, refused = [], []
    api = Server(("127.0.0.1", 0), make_api_handler(root, requests, refused))
    thread = threading.Thread(target=api.serve_forever, daemon=True)
    thread.start()
    peers = Peers(root, scenario, certs)
    env = isolated_env(root, f"http://localhost:{api.server_port}")
    for key in ENV_KEYS:
        env.pop(key, None)
    # A fail-closed loopback proxy is retained for unrelated requests; only our
    # owned peer and model API are bypassed or explicitly sent to the owned proxy.
    env.update({"HTTP_PROXY":f"http://localhost:{api.server_port}",
                "HTTPS_PROXY":f"http://localhost:{api.server_port}",
                "ALL_PROXY":f"http://localhost:{api.server_port}","NO_PROXY":"localhost,127.0.0.1"})
    env.update(peers.env)
    env.update({"CLAUDE_CODE_DISABLE_FEATURE_FLAGS":"1","CLAUDE_CODE_DISABLE_AUTO_MEMORY":"1",
                "ENABLE_TOOL_SEARCH":"false","MCP_TIMEOUT":"4000","MCP_TOOL_TIMEOUT":"4000"})
    config = {"mcpServers":{"wire network":{"type":"sse" if peers.legacy else "http",
                                                 "url":peers.url,"alwaysLoad":True}}}
    dump(root / "mcp-config.json", config)
    command = [str(binary),"--bare","-p","WIRE_NETWORK_322","--model","claude-sonnet-4-6",
        "--system-prompt","WIRE_SYSTEM","--output-format","stream-json","--verbose",
        "--setting-sources","","--strict-mcp-config","--mcp-config",str(root / "mcp-config.json"),
        "--disable-slash-commands","--no-session-persistence","--tools","",
        "--permission-mode","bypassPermissions","--dangerously-skip-permissions",
        "--debug-file",str(root / "debug.log")]
    dump(root / "invocation.json", {"command":command,"native_sha256":SHA256,"env":peers.env,
                                     "only_owned_loopback_endpoints":True,"fake_credentials":True})
    process = subprocess.Popen(command,cwd=root / "work",env=env,stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE,text=True,start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            stdout, stderr = process.communicate(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            stdout, stderr = process.communicate(timeout=3)
        (root / "stdout.jsonl").write_text(stdout)
        (root / "stderr.txt").write_text(stderr)
        raise
    finally:
        peers.close()
        api.shutdown()
        api.server_close()
        thread.join(timeout=2)
    (root / "stdout.jsonl").write_text(stdout)
    (root / "stderr.txt").write_text(stderr)
    results = [b for request in requests for b in blocks(request["body"])
               if b.get("type") == "tool_result" and b.get("tool_use_id") == "toolu_mcp_hooks_322"]
    success = bool(results and GOOD in json.dumps(results[-1]) and not results[-1].get("is_error"))
    report = {**peers.report(),"native_exit_code":process.returncode,"tool_result":results[-1] if results else None,
              "observed_success":success,"refused_api_paths":refused}
    assert success == peers.expected_success, report
    if peers.proxy and scenario != "proxy-bypass":
        assert peers.proxy_traffic, "configured proxy was not reached"
        if "connect" in scenario:
            assert all(row["method"] == "CONNECT" for row in peers.proxy_traffic)
        else:
            assert all(row["target"].startswith("http://") for row in peers.proxy_traffic)
    if scenario == "proxy-bypass":
        assert not peers.proxy_traffic
    if scenario.endswith("-basic"):
        expected = "Basic " + base64.b64encode(b"probe:pass").decode()
        assert any(dict((k.lower(),v) for k,v in row["headers"]).get("proxy-authorization") == expected
                   for row in peers.proxy_traffic)
        assert all("proxy-authorization" not in dict((k.lower(),v) for k,v in row["headers"])
                   for row in peers.traffic)
    if "mtls" in scenario and success:
        assert any(row["tls_client_certificate"] for row in peers.traffic)
    report["passed"] = True
    dump(root / "report.json", report)
    return report


def source_receipts(binary, root):
    data = binary.read_bytes()
    assert hashlib.sha256(data).hexdigest() == SHA256, "native artifact changed"
    ranges = []
    for label,start_anchor,end_anchor,lower,limit in (
        ("system-store-sources",b"class Be{ofBlock=new Map",b"class je{extraCACerts",262652000,2500),
        ("mtls-config",b"function GT(){return P().config()}",b"var it=1000",262680000,3000),
        ("ca-env-and-store",b"var kle=uN(async()=>",b"function kt(",262653000,3000),
        ("proxy-select-and-bypass",b"function Tm(e=process.env)",b"function gt(",262724000,3000),
        ("fetch-network-options",b"function _a(e){let r=process.env.API_FORCE_IDLE_TIMEOUT",b"var z=he(",262728000,2000),
        ("mcp-network-options",b"async function hce(r){if(noo()",b"\nexport{hce}",285424000,2000),
    ):
        start = data.find(start_anchor,lower)
        end = data.find(end_anchor,start+len(start_anchor))
        assert start >= lower and start < end < start+limit, label
        raw = data[start:end]
        path = root / f"{label}.js"
        path.write_bytes(raw)
        ranges.append({"label":label,"start":start,"end":end,"sha256":hashlib.sha256(raw).hexdigest(),"path":str(path)})
    dump(root / "ranges.json", {"version":VERSION,"native_sha256":SHA256,"ranges":ranges})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary",type=Path,nargs="?")
    parser.add_argument("--output",type=Path,required=True)
    parser.add_argument("--scenario",choices=("all",*SCENARIOS),default="all")
    parser.add_argument("--timeout",type=float,default=35)
    parser.add_argument("--certs",type=Path)
    parser.add_argument("--serve",action="store_true")
    args=parser.parse_args()
    args.output.mkdir(parents=True,exist_ok=True)
    certs=args.certs or args.output / "certificates"
    certificates(certs)
    if args.serve:
        assert args.scenario != "all"
        peers=Peers(args.output,args.scenario,certs)
        print(json.dumps(peers.config()),flush=True)
        try:
            sys.stdin.readline()
        finally:
            peers.close()
            dump(args.output / "report.json",peers.report())
        return
    assert args.binary is not None
    source=args.output / "source"
    source.mkdir(exist_ok=True)
    source_receipts(args.binary,source)
    reports=[]
    for scenario in SCENARIOS if args.scenario=="all" else [args.scenario]:
        report=run_native(args.binary,args.output / scenario,scenario,certs,args.timeout)
        reports.append(report)
        print(f"PASS {scenario}: success={report['observed_success']}, peer={len(report['peer_requests'])}, proxy={len(report['proxy_requests'])}",flush=True)
    dump(args.output / "report.json",{"version":VERSION,"native_sha256":SHA256,"cases":reports,
        "scope":"Actual native process, loopback fake model API, disposable TLS test certificates and loopback forward/CONNECT proxies; no external provider or real credentials."})


if __name__ == "__main__":
    main()
