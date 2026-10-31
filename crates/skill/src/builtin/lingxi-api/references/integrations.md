# SDK and protocol ownership

Choose the interface for the caller before writing an integration:

| Caller / goal | Maintained boundary |
| --- | --- |
| Rust host running an Agent | Harness `HarnessBuilder`, host services and session APIs |
| Client communicating with a model | `lingxi-llm-client` request/provider interfaces |
| Native mobile command execution | Mobile Linux API and Android/iOS platform wrappers |
| Product UI talking to its runtime | Product Bridge protocol / native FFI |
| Local App requesting Agent work | Pinned official AG-UI App Agent profile and host grants |
| Agent invoking tools in an App page | WebMCP page tool contract and host bridge |
| External tool/service integration | MCP |
| Reusable instructions/resources | Agent Skills |
| Bot-to-Bot work in a Bot-capable host | Scoped Bot MCP router and durable task state |

Read the selected revision's facade/schema and a matching compiled example.
Native-only/private product protocols are not general remote APIs or model
tools. Public source declarations alone do not resolve feature/platform cfg,
re-exports, authorization, lifecycle or actual tool registration.

The host assembles services, credentials, permission/question resolvers, event
consumption and shutdown. Keep resource ownership explicit. AG-UI events do
not grant App tools or general host administration. MCP tools and Skill prompt
expansion do not substitute for the model transport library.

For a developer checkout, resolve immutable Cargo pins with the product's
`scripts/lib/runtime_source.py` or locked metadata; do not inspect an adjacent
SDK checkout and assume it is the dependency in use. Compare the running app's
version with source/package evidence before diagnosing version-specific behavior.
Use compiler-generated Rustdoc or exact source for imports, signatures and
examples; avoid inventing another HTTP/Python/npm SDK from a Rust type name.

Source anchors: Harness `crates/runtime/src/api.rs`, `crates/client/src/protocol`,
`crates/local-app/src/v2/agent/protocol.rs`; product `packages/bridge-client`,
`packages/mobile-ffi` and its technical documentation.
