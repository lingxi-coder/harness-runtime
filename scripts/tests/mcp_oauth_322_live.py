#!/usr/bin/env python3
"""Execute immutable Claude Code MCP OAuth helpers against loopback peers.

This is controlled native-source execution, not whole-CLI login acceptance.
No browser is opened, no account or model service is contacted, and no
credential store is read or written. All tokens and client data are fake.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlencode, urlsplit

SHA256 = "a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450"

NATIVE_DRIVER = r"""
import assert from 'node:assert/strict';
import {randomBytes,webcrypto} from 'node:crypto';
import http from 'node:http';
import {writeFileSync} from 'node:fs';
import vm from 'node:vm';
const {loadNative,nativeCallbackHandler,nativeRevocation}=await import(process.argv[1]);
const base=process.argv[2],binary=process.argv[3],output=process.argv[4],missing=process.argv[5]==='missing-metadata';
const {native,sources,context,redirect}=loadNative(binary);
const realPkce=vm.runInContext(sources.pkce+';XTn',vm.createContext({TextEncoder,Uint8Array,btoa,crypto:webcrypto}),{timeout:1000});
context.XTn=realPkce;
const state=await vm.runInContext('({'+sources.state+'}).state()',vm.createContext({nt:randomBytes,le:()=>{}}),{timeout:1000});
assert.equal(Buffer.from(state,'base64url').length,32);
const serverUrl=base+'/mcp?aud=one%20two';
const isolatedFetch=(url,init={})=>{assert.equal(new URL(url).origin,base);return fetch(url,init)};
const discovery=await native.discovery(serverUrl,{fetchFn:isolatedFetch,skipIssuerMetadataValidation:true});
const metadata=discovery.authorizationServerMetadata;
assert.equal(metadata?.issuer,missing?undefined:base+'/issuer');
const resource=discovery.resourceMetadata?.resource?new URL(discovery.resourceMetadata.resource):undefined;
const callback=nativeCallbackHandler(sources,{state});
const callbackServer=http.createServer(callback.handler);
await new Promise(resolve=>callbackServer.listen(0,'127.0.0.1',resolve));
const redirectUri='http://127.0.0.1:'+callbackServer.address().port+'/callback';
const clientMetadata=native.clientMetadata({clientMetadata:{client_name:'LingXi (fake-server)',redirect_uris:[redirectUri],grant_types:['authorization_code','refresh_token'],response_types:['code'],token_endpoint_auth_method:'none'},redirectUrl:redirectUri});
const scope=native.scope({resourceMetadata:discovery.resourceMetadata,authServerMetadata:metadata,clientMetadata});
assert.equal(scope,missing?undefined:'read write offline_access');
const client=await native.register(discovery.authorizationServerUrl,{metadata,clientMetadata,scope,fetchFn:isolatedFetch});
const material=await native.authorize(discovery.authorizationServerUrl,{metadata,clientInformation:client,redirectUrl:redirectUri,scope,state,resource});
const authorizationUrl=await redirect.call({_pendingStepUpScope:false,serverConfig:{},_metadata:metadata},material.authorizationUrl);
const authorized=await isolatedFetch(authorizationUrl,{redirect:'manual'});
assert.equal(authorized.status,302);
const location=authorized.headers.get('location');
assert.equal(new URL(location).origin,new URL(redirectUri).origin);
const wrong=new URL(location);wrong.searchParams.set('state','wrong-state');
const wrongState=await fetch(wrong,{redirect:'manual'});assert.equal(wrongState.status,400);await wrongState.text();
assert.deepEqual(callback.observations,[],'wrong state must keep the listener pending');
const valid=await fetch(location,{redirect:'manual'});assert.equal(valid.status,200);await valid.text();
const code=callback.observations.find(value=>value.kind==='code');assert(code);
assert.equal(code.code,'fake +~!*code');assert.equal(code.iss,metadata?.issuer);
const tokens=await native.exchange(discovery.authorizationServerUrl,{metadata,clientInformation:client,authorizationCode:code.code,iss:code.iss,codeVerifier:material.codeVerifier,redirectUri,resource,fetchFn:isolatedFetch});
assert.equal(tokens.access_token,'fake-access');
const refreshed=await native.refresh(discovery.authorizationServerUrl,{metadata,clientInformation:client,refreshToken:tokens.refresh_token,resource:new URL(serverUrl),fetchFn:isolatedFetch});
assert.equal(refreshed.access_token,'fake-access');
let rejected;
try{await native.refresh(discovery.authorizationServerUrl,{metadata:{...metadata,token_endpoint:base+'/token?error=invalid_grant'},clientInformation:client,refreshToken:'fake-rejected',fetchFn:isolatedFetch})}catch(error){rejected=error.message}
assert(rejected?.includes('400'),'HTTP token error must reject');
const revoke=nativeRevocation(sources,async(url,body,options)=>{
 const response=await isolatedFetch(url,{method:'POST',body,headers:options.headers});
 if(!response.ok)throw Object.assign(Error('fake unauthorized'),{isAxiosError:true,response:{status:response.status}});
});
await revoke({serverName:'fake-server',endpoint:base+'/revoke',token:'fake-refresh',tokenTypeHint:'refresh_token',clientId:client.client_id,accessToken:'fake-access',configuredHeaders:{}});
const denied=nativeCallbackHandler(sources,{state:'denied-state'});
const deniedServer=http.createServer(denied.handler);await new Promise(resolve=>deniedServer.listen(0,'127.0.0.1',resolve));
const deniedUrl='http://127.0.0.1:'+deniedServer.address().port+'/callback?state=denied-state&error=access_denied&error_description=fake%20refusal&error_uri=https%3A%2F%2Fas.example%2Ferror';
const deniedResponse=await fetch(deniedUrl);assert.equal(deniedResponse.status,200);await deniedResponse.text();
assert.equal(denied.observations.find(value=>value.kind==='error').message,'OAuth error: access_denied - fake refusal (See: https://as.example/error)');
// Cancelling the owner's pending listener is exercised without token exchange.
const cancelled=nativeCallbackHandler(sources,{state:'cancel-state'});
const cancelServer=http.createServer(cancelled.handler);await new Promise(resolve=>cancelServer.listen(0,'127.0.0.1',resolve));
await new Promise(resolve=>cancelServer.close(resolve));assert.deepEqual(cancelled.observations,[]);
callbackServer.closeAllConnections();deniedServer.closeAllConnections();
await Promise.all([new Promise(resolve=>callbackServer.close(resolve)),new Promise(resolve=>deniedServer.close(resolve))]);
const result={scenario:missing?'missing-metadata':'metadata',mode:'Controlled execution of immutable native JavaScript helpers over real loopback HTTP. Schema/error constructors are adapters; callback page renderer is a JSON adapter. Not whole-CLI OAuth acceptance.',serverUrl,redirectUri,scope,authorizationUrl,verifier:material.codeVerifier,state,callback:callback.observations,wrongStateStatus:wrongState.status,tokenAccess:tokens.access_token,refreshedAccess:refreshed.access_token,httpError:rejected,denied:denied.observations,cancelledBeforeExchange:true};
writeFileSync(output,JSON.stringify(result,null,2)+'\n');
console.log('native-source-loopback OAuth flow passed');
"""


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path.home() / ".local/share/claude/versions/2.1.322")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--scenario", choices=("metadata", "missing-metadata"), default="metadata")
    args = parser.parse_args()
    binary = args.binary.resolve()
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == SHA256
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    traffic: list[dict] = []
    lock = threading.Lock()
    authorization: dict[str, list[str]] = {}

    def dump(name: str, value: object) -> None:
        (root / name).write_text(json.dumps(value, ensure_ascii=True, indent=2) + "\n")

    class Peer(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):
            pass

        def record(self) -> bytes:
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            with lock:
                index = len(traffic) + 1
                traffic.append({"index": index, "method": self.command, "path": self.path,
                                "request_line": self.requestline, "headers": list(self.headers.items()),
                                "body_utf8": raw.decode(), "body_sha256": hashlib.sha256(raw).hexdigest()})
                (root / f"request-{index:02d}.raw").write_bytes(raw)
                dump("traffic.json", traffic)
            return raw

        def reply(self, status: int, body: object | None = None, **headers: str) -> None:
            raw = b"" if body is None else json.dumps(body, separators=(",", ":")).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            for name, value in headers.items():
                self.send_header(name, value)
            self.end_headers()
            self.wfile.write(raw)

        def do_GET(self):
            self.record()
            parsed = urlsplit(self.path)
            if args.scenario == "missing-metadata" and ".well-known/" in parsed.path:
                return self.reply(404)
            if parsed.path in ("/.well-known/oauth-protected-resource/mcp", "/.well-known/oauth-authorization-server/issuer", "/.well-known/openid-configuration/issuer"):
                return self.reply(404)
            if parsed.path == "/.well-known/oauth-protected-resource":
                return self.reply(200, {"resource": base + "/mcp", "authorization_servers": [base + "/issuer"], "scopes_supported": ["read", "write"]})
            if parsed.path == "/issuer/.well-known/openid-configuration":
                return self.reply(200, {"issuer": base + "/issuer", "authorization_endpoint": base + "/authorize",
                                       "token_endpoint": base + "/token", "registration_endpoint": base + "/register",
                                       "revocation_endpoint": base + "/revoke", "response_types_supported": ["code"],
                                       "scopes_supported": ["read", "write", "offline_access"], "code_challenge_methods_supported": ["S256"],
                                       "token_endpoint_auth_methods_supported": ["none"], "authorization_response_iss_parameter_supported": True})
            if parsed.path == "/authorize":
                authorization.update(parse_qs(parsed.query))
                if args.scenario == "metadata":
                    assert authorization["scope"] == ["read write offline_access"]
                    assert authorization["prompt"] == ["consent"]
                else:
                    assert "scope" not in authorization and "prompt" not in authorization
                callback_query = {"code": "fake +~!*code", "state": authorization["state"][0]}
                if args.scenario == "metadata":
                    callback_query["iss"] = base + "/issuer"
                callback = authorization["redirect_uri"][0] + "?" + urlencode(callback_query)
                return self.reply(302, Location=callback)
            self.reply(404)

        def do_POST(self):
            raw = self.record()
            parsed = urlsplit(self.path)
            if parsed.path == "/register":
                body = json.loads(raw)
                assert body["client_name"] == "LingXi (fake-server)"
                if args.scenario == "metadata":
                    assert body["scope"] == "read write offline_access"
                else:
                    assert "scope" not in body
                assert body["application_type"] == "native"
                return self.reply(201, {"client_id": "fake-dcr-client"})
            form = parse_qs(raw.decode(), keep_blank_values=True)
            if parsed.path == "/token":
                if parsed.query:
                    return self.reply(400, {"error": "invalid_grant", "error_description": "fake rejected refresh"})
                assert form["client_id"] == ["fake-dcr-client"]
                if form["grant_type"] == ["authorization_code"]:
                    import base64
                    challenge = base64.urlsafe_b64encode(hashlib.sha256(form["code_verifier"][0].encode()).digest()).decode().rstrip("=")
                    assert [challenge] == authorization["code_challenge"]
                    if args.scenario == "metadata":
                        assert form["resource"] == [base + "/mcp"]
                    else:
                        assert "resource" not in form
                    assert form["code"] == ["fake +~!*code"]
                else:
                    assert form["refresh_token"] == ["fake-refresh"]
                return self.reply(200, {"access_token": "fake-access", "refresh_token": "fake-refresh", "token_type": "Bearer", "expires_in": 60})
            if parsed.path == "/revoke":
                assert form["token"] == ["fake-refresh"]
                if self.headers.get("Authorization") == "Bearer fake-access":
                    assert "client_id" not in form
                    return self.reply(200)
                return self.reply(401)
            self.reply(404)

    peer = ThreadingHTTPServer(("127.0.0.1", 0), Peer)
    base = f"http://127.0.0.1:{peer.server_port}"
    threading.Thread(target=peer.serve_forever, daemon=True).start()
    oracle = (Path(__file__).resolve().parent / "mcp_oauth_322_oracle.mjs").as_uri()
    result = None
    try:
        env = {"PATH": os.environ.get("PATH", os.defpath), "HOME": str(root), "TMPDIR": str(root), "NO_PROXY": "127.0.0.1,localhost"}
        process = subprocess.run(["node", "--input-type=module", "-e", NATIVE_DRIVER, oracle, base, str(binary), str(root / "native-result.json"), args.scenario], cwd=root, env=env, capture_output=True, text=True, timeout=30)
        (root / "native.stdout").write_text(process.stdout)
        (root / "native.stderr").write_text(process.stderr)
        assert process.returncode == 0, process.stderr
        result = json.loads((root / "native-result.json").read_text())
        paths = [urlsplit(row["path"]).path for row in traffic]
        discovery_paths = (["/.well-known/oauth-protected-resource/mcp", "/.well-known/oauth-protected-resource",
                            "/.well-known/oauth-authorization-server/issuer", "/.well-known/openid-configuration/issuer",
                            "/issuer/.well-known/openid-configuration"] if args.scenario == "metadata" else
                           ["/.well-known/oauth-protected-resource/mcp", "/.well-known/oauth-protected-resource",
                            "/.well-known/oauth-authorization-server", "/.well-known/openid-configuration"])
        assert paths == discovery_paths + ["/register", "/authorize", "/token", "/token", "/token", "/revoke", "/revoke"], paths
        for row in traffic[:len(discovery_paths)]:
            headers = {key.lower(): value for key, value in row["headers"]}
            assert headers["mcp-protocol-version"] == "2025-11-25"
        assert traffic[0]["path"].endswith("?aud=one%20two")
        assert traffic[1]["path"] == "/.well-known/oauth-protected-resource"
        assert "code=fake+%2B%7E%21*code&code_verifier=" in traffic[len(discovery_paths) + 2]["body_utf8"]
        assert traffic[len(discovery_paths) + 3]["body_utf8"].startswith("grant_type=refresh_token&refresh_token=fake-refresh&resource=")
        dump("receipt.json", {"native_sha256": SHA256, "mode": "hash-pinned native-source execution over real loopback sockets",
                              "scenario": args.scenario, "whole_cli_login": False, "real_credentials_or_store_access": False,
                              "requests": len(traffic), "wrong_state_keeps_listener": result["wrongStateStatus"] == 400,
                              "pkce_checked_by_peer": True, "token_refresh": True, "revocation_401_bearer_retry": True,
                              "provider_error": True, "owner_listener_cancel": True,
                              "source_oracle": str(Path(__file__).resolve().parent / "mcp_oauth_322_oracle.mjs"),
                              "limits": ["Schema/error constructors are explicit adapters", "Callback page renderer is a JSON adapter", "Owner cancellation is local server shutdown, not a whole-CLI signal test"]})
        print(json.dumps({"status": "passed", "requests": len(traffic), "output": str(root)}))
    finally:
        peer.shutdown()
        peer.server_close()


if __name__ == "__main__":
    main()
