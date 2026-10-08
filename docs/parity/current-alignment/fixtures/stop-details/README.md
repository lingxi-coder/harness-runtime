# Native refusal stop-details fixture

This fixture sends a fixed refusal response through the public Claude Code
2.1.293 CLI. The Anthropic SSE stream has fixed message and session IDs,
visible text, `stop_reason: "refusal"`, and a `stop_details.explanation`
containing one isolated UTF-16 high surrogate.

Run it with the pinned executable:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude python3 docs/parity/current-alignment/fixtures/stop-details/run-native.py
CLAUDE_STOP_DETAILS_CASE=refusal-generic CLAUDE_NATIVE_BINARY=/path/to/claude python3 docs/parity/current-alignment/fixtures/stop-details/run-native.py
```

The runner checks the 2.1.293 binary hash, uses an isolated HOME/config and a
fixed dummy key, serves only on 127.0.0.1, enables transcript persistence, and
keeps the original SSE response, stdout, stderr, and full session JSONL under
ignored `run-output/<case>/`. Choose `refusal-cyber` (the default) or
`refusal-generic` with no category. Each receipt reports exact explanation
code units found in the parsed Native result/session and hashes raw artifacts;
it does not regenerate raw artifacts by parsing and serializing them. Each
checked-in baseline keeps only original assistant rows containing
`stop_details`.

Native 2.1.293's cyber case made two `stream:true` requests and persisted
three assistant rows containing the explanation's exact UTF-16 units,
including U+D800. Its final JSON result contains the cyber safeguard text,
`Details: [cyber]`, and the fixed message ID, but none of the explanation, its
lone unit, U+FFFD, or a literal `\\ud800`.

The no-category case also made two `stream:true` requests and persisted three
rows with the same explanation units. Its exact final result is “API Error:
Sonnet 4.5 can't help with this. Start a new session to continue.” followed by
the Anthropic usage-policy link and fixed message ID; it also omits the
explanation. Both fixtures use `claude-sonnet-4-5` (Native label `Sonnet 4.5`)
in non-interactive `--print --output-format json` mode. They do not test an
unresolved display label or interactive output. Each Native CLI exit code is
recorded; the runner itself exits successfully after recording Native
outcomes.

This is Native-only evidence: Harness and SDK-source replay remain separate
gates. The result shows that Native's persisted row keeps source units even
though this CLI presentation does not show the explanation. It does not imply
that all stop-detail categories or outputs behave the same way.

Claude Code 2.1.294 is recorded separately in the current matrix. The existing
Native hook-regexp corpus covers matcher semantics, not the release's prompt
hook instruction-judgement change, so no 2.1.294 hook regression is claimed.
