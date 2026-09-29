#!/usr/bin/env python3
"""Reject production model/provider networking outside the shared SDK.

General HTTP (web downloads, public search engines, MCP OAuth and telemetry)
remains host-owned. This gate scans endpoint construction and removed adapter
symbols, with test-only Rust items excluded. It complements runtime call-path tests.
"""
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
# These are fixtures, not permitted production bypasses.
NETWORK_EXCEPTIONS = {
    "crates/http-client/src/monitor_websocket.rs",  # Monitor channel.
    "crates/bridge/src/mcp_endpoint.rs",  # MCP server.
    "crates/platforms/common/src/mcp_ws.rs",  # MCP client.
}
# This module serializes normalized host events for the CLI, not provider input.
STREAM_EXCEPTIONS = {"crates/orchestrator/src/sse/event_router.rs"}
FIXTURES = {"crates/llm-runtime/src/test_support.rs", "crates/tool-api/src/test_support.rs", "crates/llm-runtime/src/codec_fixtures.rs", "crates/llm-runtime/src/codec_tests/gemini_files_fixture.rs"}

def item_end(source, start):
    """Find the end of a cfg-gated Rust item without counting quoted braces."""
    raw_string = re.compile(r'r(#+)?"')
    character = re.compile(r"'(?:\\.|[^'\\])'")
    index, depth = start, 0
    while index < len(source):
        if source.startswith('//', index):
            stop = source.find('\n', index)
            index = len(source) if stop < 0 else stop
            continue
        if source.startswith('/*', index):
            nesting = 1
            index += 2
            while nesting and index < len(source):
                if source.startswith('/*', index):
                    nesting += 1
                    index += 2
                elif source.startswith('*/', index):
                    nesting -= 1
                    index += 2
                else:
                    index += 1
            continue
        raw = raw_string.match(source, index)
        if raw:
            marker = '"' + (raw.group(1) or '')
            index = source.index(marker, raw.end()) + len(marker)
            continue
        if source[index] == '"':
            index += 1
            while index < len(source):
                if source[index] == '\\':
                    index += 2
                elif source[index] == '"':
                    index += 1
                    break
                else:
                    index += 1
            continue
        char = character.match(source, index)
        if char:
            index = char.end()
            continue
        if source[index] == ';' and depth == 0:
            return index + 1
        if source[index] == '{':
            depth += 1
        elif source[index] == '}':
            depth -= 1
            if depth == 0:
                return index + 1
        index += 1
    raise ValueError('unbalanced cfg-gated Rust item')

def production(source):
    cfg = re.compile(r'^[ \t]*#\[cfg\((?:test|any\(test,\s*feature\s*=\s*"test-support"\))\)\]', re.M)
    masked_until = 0
    for match in cfg.finditer(source):
        if match.start() < masked_until:
            continue
        stop = item_end(source, match.end())
        source = source[:match.start()] + re.sub(r'[^\n]', ' ', source[match.start():stop]) + source[stop:]
        masked_until = stop
    tokens = re.compile(r'r(\#*)"[\s\S]*?"\1|"(?:\\.|[^"\\])*"|//[^\n]*|/\*[\s\S]*?\*/')
    return tokens.sub(lambda m: re.sub(r'[^\n]', ' ', m.group()) if m.group().startswith(('//', '/*')) else m.group(), source)

RULES = {
    "provider quota headers belong in llm-client": re.compile(r'"x-claudeai-window-(?:count|limit)"'),

    "provider authentication endpoint belongs in llm-client": re.compile(r'"[^"\n]*(?:/oauth/token|/oauth/authorize|/api/oauth/(?:profile|roles)|/login/device/code|/login/oauth/access_token|/api/accounts/deviceauth/|/copilot_internal/v2/token|/user-auth-credential/whoami)[^"\n]*"'),
    "model endpoint belongs in llm-client": re.compile(r'"[^"\n]*(?:/v1/messages|/chat/completions|/backend-api/models|/responses|/embeddings|/audio/(?:speech|transcriptions|translations)|/images/(?:generations|edits)|/v1/models|:generateContent|:streamGenerateContent)[^"\n]*"'),
    "provider stream parsing belongs in llm-client": re.compile(r'"(?:content_block_(?:start|delta|stop)|response\.(?:output_text|output_item|function_call_arguments)\.[^"\n]+)"'),
    "WebSocket implementation requires an explicit non-model exception": re.compile(r'\btokio_tungstenite\s*::'),
    "removed model adapter must not return": re.compile(r'\b(?:struct\s+AnthropicRequestBuilder|struct\s+LlmTransportBridge|trait\s+ResponsesWebSocketTransportSession|struct\s+StreamReassembler|struct\s+DefaultLlmClient|struct\s+ResponsesWebSocketSession|trait\s+CopilotHttp|struct\s+PosixCopilotHttp|trait\s+WireCodec|trait\s+StreamDecoder|struct\s+LlmResponse|enum\s+LlmEvent)\b'),
}

RUNTIME_RULES = {
    "host OAuth client forwarding objects must not return": re.compile(r'\bstruct\s+(?:ClaudeAiOAuthClient|OpenAiOAuthClient)\b'),
    "single provider auth operations must call the SDK directly": re.compile(r'\bfn\s+(?:request_device_code|poll_for_token|whoami|obtain_api_key|build_authorize_url(?:_with_redirect|_with_options|_pair_with_options)?)\b'),
    "canonical usage and pricing types belong in llm-client": re.compile(r'\bstruct\s+(?:Usage|TokenUsage|ServerToolUsage|TokenPricing)\b'),
    "provider usage normalization belongs in llm-client": re.compile(r'\bfn\s+normalize_anthropic_usage\b'),
    "provider credential parsing belongs in llm-client": re.compile(r'\bfn\s+parse_sts_output\b'),
}

def runtime_findings(source):
    text = production(source)
    return [(text.count('\n', 0, match.start()) + 1, reason) for reason, rule in RUNTIME_RULES.items() for match in rule.finditer(text)]

def findings(source):
    text = production(source)
    return [(text.count('\n', 0, match.start()) + 1, reason) for reason, rule in RULES.items() for match in rule.finditer(text)]

def main():
    if "--selftest" in sys.argv:
        assert findings('fn run() { let url = format!("{base}/v1/messages"); }')
        assert findings('fn refresh() { let url = "https://auth.openai.com/oauth/token"; }')
        assert findings('fn device() { let url = format!("https://{host}/login/device/code"); }')
        assert not findings('fn callback() { let url = "http://localhost:1455/auth/callback"; }')
        assert findings('struct AnthropicRequestBuilder {}')
        assert findings('pub trait CopilotHttp {}')
        assert findings('pub trait WireCodec {}')
        assert findings('pub trait StreamDecoder {}')
        assert findings('pub struct DefaultLlmClient {}')
        assert findings('pub struct LlmResponse {}')
        assert not findings('pub use lingxi_llm_client::WireCodec;')
        assert findings('match event { \"content_block_delta\" => {} }')
        assert findings('fn connect(){ tokio_tungstenite::connect_async(url); }')
        assert findings('fn run(){ let url="https://provider.test/v1/messages"; }')
        assert not findings('#[cfg(test)] mod tests { fn mock() { let url="/v1/messages"; } }')
        assert findings('#[cfg(test)] mod tests {}\nfn later(){let x="/chat/completions";}')
        assert not findings('fn web() { let url="https://example.test/page"; }')
        assert runtime_findings('pub struct Usage { input: u64 }')
        assert runtime_findings('pub fn normalize_anthropic_usage() {}')
        assert runtime_findings('pub fn parse_sts_output() {}')
        assert runtime_findings('pub struct OpenAiOAuthClient {}')
        assert runtime_findings('pub async fn whoami() {}')
        assert not runtime_findings('sdk::whoami(transport, config, token).await')
        assert not runtime_findings('pub async fn run_device_code_login() {}')
        assert not runtime_findings('pub struct ExecutionUsage { report: sdk::UsageReport }')
        assert not runtime_findings('pub use sdk::protocol::Usage;')
        assert findings('fn parse() { headers.get("x-claudeai-window-limit"); }')
        print('llm-boundary selftest: OK')
        return 0
    errors = []
    removed_modules = [
        "oauth", "credential_lifecycle", "aws_auth.rs", "credentials.rs", "copilot.rs",
        "auth/anthropic/client.rs", "auth/openai/client.rs",
        "anthropic.rs", "sigv4.rs", "copilot/auth.rs", "copilot/login.rs",
        "oauth/anthropic/config.rs", "oauth/anthropic/pkce.rs", "oauth/anthropic/profile.rs",
        "oauth/openai/config.rs", "oauth/openai/pkce.rs", "oauth/openai/token_data.rs",
    ]
    for relative in removed_modules:
        if (ROOT / "crates/llm-runtime/src" / relative).exists():
            errors.append(f"crates/llm-runtime/src/{relative}: removed SDK forwarding module must not return")
    for path in sorted((ROOT / 'crates').rglob('*.rs')):
        relative = path.relative_to(ROOT).as_posix()
        if relative in FIXTURES or '/tests/' in relative or path.stem.endswith(('_test', '_tests')):
            continue
        source = path.read_text()
        if relative.startswith("crates/llm-runtime/src/"):
            errors.extend(f'{relative}:{line}: {reason}' for line, reason in runtime_findings(source))
        errors.extend(f'{relative}:{line}: {reason}' for line, reason in findings(source)
                      if not (relative in STREAM_EXCEPTIONS and reason == 'provider stream parsing belongs in llm-client')
                      and not (relative in NETWORK_EXCEPTIONS and reason.startswith('WebSocket implementation')))
    if errors:
        print('\n'.join(errors), file=sys.stderr)
        return 1
    print('llm-boundary: OK — no production model endpoint or removed adapter bypass')
    return 0

if __name__ == '__main__':
    raise SystemExit(main())
