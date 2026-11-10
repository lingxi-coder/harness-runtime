#!/usr/bin/env python3
"""Probe SDK MCP authentication with SBPL-denied host credentials and local OAuth."""
import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import sys
import threading
import time
from urllib.parse import parse_qs, urlsplit, urlencode

SHA = "a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path.home()/".local/share/claude/versions/2.1.322")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--redirect-uri")
    parser.add_argument("--redirect-uri-only", action="store_true")
    parser.add_argument("--callback-mode", choices=["control", "loopback", "cancel"], default="control")
    parser.add_argument("--reject-register", choices=["always", "custom"])
    parser.add_argument("--token-error", action="store_true")
    parser.add_argument("--proxy-server", action="store_true")
    parser.add_argument("--reconnect-after-callback", action="store_true")
    args = parser.parse_args()
    assert hashlib.sha256(args.binary.read_bytes()).hexdigest() == SHA
    profile = args.profile.resolve()
    assert '(deny mach-lookup)' in profile.read_text()
    assert '(literal "/usr/bin/security")' in profile.read_text()
    assert '(literal "/usr/bin/open")' in profile.read_text()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    for name in ["cwd", "config", "tmp", "bin"]:
        (root/name).mkdir()
    for name in ["security", "open"]:
        shim = root/"bin"/name
        shim.write_text("#!/bin/sh\nexit 44\n")
        shim.chmod(0o755)
    env = {k:v for k,v in os.environ.items() if k in ["HOME", "LANG", "LC_ALL"]}
    env.update({"PATH":str(root/"bin")+":/usr/bin:/bin:/usr/sbin:/sbin", "TERM":"dumb", "TMPDIR":str(root/"tmp"),
                "CLAUDE_CONFIG_DIR":str(root/"config"), "ANTHROPIC_API_KEY":"sk-ant-offline-sdk-auth", "ANTHROPIC_BASE_URL":"http://127.0.0.1:9",
                "HTTP_PROXY":"http://127.0.0.1:9", "HTTPS_PROXY":"http://127.0.0.1:9", "ALL_PROXY":"http://127.0.0.1:9",
                "NO_PROXY":"127.0.0.1,localhost", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC":"1", "DISABLE_AUTOUPDATER":"1",
                "DISABLE_TELEMETRY":"1", "DISABLE_ERROR_REPORTING":"1", "MCP_PROTOCOL_NEGOTIATION":"auto"})
    prefix = ["/usr/bin/sandbox-exec", "-f", str(profile)]
    proof = []
    canary = profile.parent/"credential-canary"
    for label, command in [("canary", ["/bin/cat", str(canary)]), ("security-exec", ["/usr/bin/security", "-h"]), ("browser-exec", ["/usr/bin/open", "http://127.0.0.1:9/"]), ("native-version", [str(args.binary), "--version"])]:
        result = subprocess.run(prefix+command, cwd=root/"cwd", env=env, capture_output=True, timeout=20)
        proof.append({"label":label,"exitCode":result.returncode,"stdout":result.stdout.decode(errors="replace"),"stderr":result.stderr.decode(errors="replace")})
    (root/"isolation-proof.json").write_text(json.dumps(proof,indent=2)+"\n")
    assert all(item["exitCode"] != 0 and "Operation not permitted" in item["stderr"] for item in proof[:3]), proof
    assert proof[3]["exitCode"] == 0
    traffic = []
    lock = threading.Lock()

    class Peer(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def record(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            with lock:
                traffic.append({"method":self.command,"path":self.path,"headers":list(self.headers.items()),"body":body.decode()})
                (root/"traffic.json").write_text(json.dumps(traffic,indent=2)+"\n")
            return body

        def reply(self, status, value=None, **headers):
            body = b"" if value is None else json.dumps(value,separators=(",",":")).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            for key,value in headers.items(): self.send_header(key.replace("_","-"),value)
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            self.record()
            path = urlsplit(self.path).path
            if path.startswith("/.well-known/oauth-protected-resource"):
                return self.reply(200,{"resource":base+"/mcp","authorization_servers":[base],"scopes_supported":["read","write"]})
            if path == "/.well-known/oauth-authorization-server":
                return self.reply(200,{"issuer":base,"authorization_endpoint":base+"/authorize","token_endpoint":base+"/token","registration_endpoint":base+"/register","response_types_supported":["code"],"grant_types_supported":["authorization_code","refresh_token"],"code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"],"scopes_supported":["read","write","offline_access"]})
            if path == "/mcp": return self.reply(405)
            return self.reply(404)

        def do_POST(self):
            raw = self.record()
            if self.path == "/register":
                registration=json.loads(raw)
                if args.reject_register == "always" or args.reject_register == "custom" and not registration["redirect_uris"][0].startswith("http://localhost:"):
                    return self.reply(400,{"error":"invalid_redirect_uri","error_description":"LOCAL_REDIRECT_REJECTED"})
                return self.reply(201,{**registration,"client_id":"fake-sdk-auth-client"})
            if self.path == "/token":
                if args.token_error:return self.reply(400,{"error":"invalid_grant","error_description":"LOCAL_GRANT_REJECTED"})
                return self.reply(200,{"access_token":"fake-sdk-auth-token","refresh_token":"fake-sdk-auth-refresh","token_type":"Bearer","expires_in":3600,"scope":"read write offline_access"})
            if self.path == "/mcp":
                if self.headers.get("Authorization") != "Bearer fake-sdk-auth-token":
                    return self.reply(401,{"error":"unauthorized"},WWW_Authenticate='Bearer resource_metadata="'+base+'/.well-known/oauth-protected-resource"')
                frame = json.loads(raw)
                if "id" not in frame:return self.reply(202)
                if frame["method"] == "initialize":result={"protocolVersion":frame["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"fake-sdk-auth","version":"1"}}
                elif frame["method"] == "tools/list":result={"tools":[]}
                else:result={}
                return self.reply(200,{"jsonrpc":"2.0","id":frame["id"],"result":result})
            return self.reply(404)

    peer = ThreadingHTTPServer(("127.0.0.1",0),Peer)
    peer.daemon_threads = True
    base = "http://127.0.0.1:"+str(peer.server_port)
    threading.Thread(target=peer.serve_forever,daemon=True).start()
    stdio = root/"stdio.py"
    stdio.write_text('import json,sys\nfor line in sys.stdin:\n m=json.loads(line)\n if "id" not in m:continue\n r={"protocolVersion":m.get("params",{}).get("protocolVersion","2025-11-25"),"capabilities":{},"serverInfo":{"name":"fake-stdio","version":"1"}} if m.get("method")=="initialize" else {}\n print(json.dumps({"jsonrpc":"2.0","id":m["id"],"result":r}),flush=True)\n')
    config = {"mcpServers":{"oauth raw Ω":{"type":"http","url":base+"/mcp"},"stdio raw Ω":{"type":"stdio","command":sys.executable,"args":[str(stdio)]}}}
    (root/"mcp.json").write_text(json.dumps(config,ensure_ascii=True)+"\n")
    command = prefix+[str(args.binary),"--bare","-p","--input-format","stream-json","--output-format","stream-json","--verbose","--permission-mode","bypassPermissions","--strict-mcp-config","--mcp-config",str(root/"mcp.json")]
    process = subprocess.Popen(command,cwd=root/"cwd",env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
    frames = queue.Queue()
    report = {"nativeSha256":SHA,"profile":str(profile),"profileSha256":hashlib.sha256(profile.read_bytes()).hexdigest(),"isolationProved":True,"frames":[],"responses":{},"requests":[]}

    def read_stdout():
        with (root/"native.stdout.jsonl").open("ab") as out:
            for raw in process.stdout:
                out.write(raw);out.flush()
                try:frames.put(json.loads(raw))
                except Exception:frames.put({"raw":raw.decode("utf8","replace")})
        frames.put({"eof":process.poll()})

    def read_stderr():
        with (root/"native.stderr").open("ab") as out:
            for raw in process.stderr:out.write(raw);out.flush()

    threading.Thread(target=read_stdout,daemon=True).start()
    threading.Thread(target=read_stderr,daemon=True).start()

    def send(frame):
        raw=(json.dumps(frame,ensure_ascii=True,separators=(",",":"))+"\n").encode()
        with (root/"host.stdin.jsonl").open("ab") as out:out.write(raw)
        process.stdin.write(raw);process.stdin.flush()

    def request(rid, body):
        report["requests"].append({"id":rid,"request":body})
        send({"type":"control_request","request_id":rid,"request":body})

    def wait(rid, timeout=20):
        deadline=time.monotonic()+timeout
        while rid not in report["responses"]:
            frame=frames.get(timeout=max(.1,deadline-time.monotonic()))
            report["frames"].append(frame)
            if "eof" in frame:raise RuntimeError(frame)
            if frame.get("type")=="control_response":report["responses"][frame["response"]["request_id"]]=frame["response"]
            if frame.get("request",{}).get("subtype")=="mcp_message":
                message=frame["request"]["message"]
                if message.get("method")=="initialize":result={"protocolVersion":message["params"]["protocolVersion"],"capabilities":{},"serverInfo":{"name":"sdk raw Ω","version":"1"}}
                else:result={}
                send({"type":"control_response","response":{"subtype":"success","request_id":frame["request_id"],"response":{"mcp_response":{"jsonrpc":"2.0","id":message.get("id",0),"result":result}}}})
        return report["responses"][rid]

    try:
        request("initialize",{"subtype":"initialize","sdkMcpServers":["sdk raw Ω"]});wait("initialize")
        for attempt in range(30):
            rid="status-"+str(attempt);request(rid,{"subtype":"mcp_status"});result=wait(rid)
            rows=result.get("response",{}).get("mcpServers",[])
            if any(row.get("name")=="oauth raw Ω" for row in rows):break
            time.sleep(.05)
        for rid,name in [("missing","missing raw Ω"),("sdk","sdk raw Ω"),("stdio","stdio raw Ω")]:
            request(rid,{"subtype":"mcp_authenticate","serverName":name});wait(rid)
        if args.proxy_server:
            request("set-proxy",{"subtype":"mcp_set_servers","servers":{"oauth raw Ω":{"type":"claudeai-proxy","url":base+"/mcp","id":"fake-sdk-proxy-id"}}});wait("set-proxy")
        auth_request={"subtype":"mcp_authenticate","serverName":"oauth raw Ω"}
        if args.redirect_uri is not None:auth_request["redirectUri"]=args.redirect_uri
        if args.redirect_uri_only:auth_request["redirectUriOnly"]=True
        request("oauth",auth_request);wait("oauth")
        assert report["responses"]["missing"]["error"] == "Server not found: missing raw Ω"
        assert report["responses"]["sdk"]["error"] == 'Server type "sdk" does not support OAuth authentication'
        assert report["responses"]["stdio"]["error"] == 'Server type "stdio" does not support OAuth authentication'
        auth=report["responses"]["oauth"].get("response",{})
        if args.proxy_server:
            assert report["responses"]["oauth"]["error"] == "Unable to build claude.ai connector auth URL (missing org or server id)"
        elif args.reject_register == "always" or args.reject_register == "custom" and args.redirect_uri_only:
            assert report["responses"]["oauth"]["error_code"] == ("mcp_redirect_uri_unusable" if args.redirect_uri_only else "mcp_client_registration_unavailable")
        else:
            assert auth["requiresUserAction"] is True and auth["callbackExpected"] is True
            assert bool(auth.get("callbackPort")) == (auth["redirectScheme"] == "localhost")
        if auth.get("authUrl"):
            parsed=parse_qs(urlsplit(auth["authUrl"]).query)
            callback=parsed["redirect_uri"][0]+"?"+urlencode({"code":"fake-sdk-auth-code","state":parsed["state"][0]})
            if args.callback_mode == "loopback":
                import http.client
                target=urlsplit(callback)
                connection=http.client.HTTPConnection(target.hostname,target.port,timeout=10)
                connection.request("GET",target.path+"?"+target.query)
                reply=connection.getresponse();report["loopbackCallback"]={"status":reply.status,"body":reply.read().decode()};connection.close()
            elif args.callback_mode == "cancel":
                send({"type":"control_cancel_request","request_id":"oauth"})
            else:
                request("callback-invalid",{"subtype":"mcp_oauth_callback_url","serverName":"oauth raw Ω","callbackUrl":"lingxi-test://callback"});wait("callback-invalid")
                request("callback-wrong-state",{"subtype":"mcp_oauth_callback_url","serverName":"oauth raw Ω","callbackUrl":parsed["redirect_uri"][0]+"?code=fake&state=wrong"});wait("callback-wrong-state")
                request("callback",{"subtype":"mcp_oauth_callback_url","serverName":"oauth raw Ω","callbackUrl":callback});wait("callback")
            request("status-after",{"subtype":"mcp_status"});wait("status-after")
            time.sleep(.5)
            request("status-settled",{"subtype":"mcp_status"});wait("status-settled")
            if args.callback_mode == "cancel":
                request("callback-after-cancel",{"subtype":"mcp_oauth_callback_url","serverName":"oauth raw Ω","callbackUrl":callback});wait("callback-after-cancel")
            if args.callback_mode == "control":
                assert report["responses"]["callback-invalid"]["error"] == "Invalid callback URL: missing authorization code. Please paste the full redirect URL including the code parameter."
                assert report["responses"]["callback-wrong-state"]["error"] == "Callback URL not accepted: it belongs to a different sign-in attempt (OAuth state mismatch) or carries no authorization code. The sign-in in progress is still waiting; submit the redirect URL from its own browser page."
                if args.token_error:assert report["responses"]["callback"]["error"] == "LOCAL_GRANT_REJECTED"
                else:assert report["responses"]["callback"] == {"subtype":"success","request_id":"callback"}
            if args.callback_mode == "cancel":assert report["responses"]["callback-after-cancel"] == {"subtype":"success","request_id":"callback-after-cancel"}
            if args.reconnect_after_callback:
                request("reconnect",{"subtype":"mcp_reconnect","serverName":"oauth raw Ω"});wait("reconnect")
                assert report["responses"]["reconnect"] == {"subtype":"success","request_id":"reconnect"}
                for attempt in range(30):
                    rid="status-reconnect-"+str(attempt);request(rid,{"subtype":"mcp_status"});result=wait(rid)
                    if any(row.get("name")=="oauth raw Ω" and row.get("status")=="connected" for row in result.get("response",{}).get("mcpServers",[])):break
                    time.sleep(.05)
                else:raise AssertionError("manual callback grant must survive explicit SDK reconnect")
        report["passed"]=True
    finally:
        (root/"report.json").write_text(json.dumps(report,ensure_ascii=True,indent=2)+"\n")
        if process.poll() is None:os.killpg(process.pid,signal.SIGTERM)
        try:process.wait(timeout=5)
        except subprocess.TimeoutExpired:os.killpg(process.pid,signal.SIGKILL);process.wait()
        peer.shutdown()
    print(json.dumps({"passed":report.get("passed",False),"responses":{key:value for key,value in report["responses"].items() if key!="initialize" and not key.startswith("status-")},"receipt":str(root/"report.json")},ensure_ascii=True))


if __name__ == "__main__":main()
