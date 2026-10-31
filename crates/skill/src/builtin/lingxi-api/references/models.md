# Models, provider profiles and llm-client

LingXi supports multiple provider profiles. A profile identifies the connection,
authentication strategy and model catalog; a model name by itself is not a
complete usable connection. Inspect the current host's model/settings entry
when selection or authentication matters. An installed app does not imply a
paid API account, working credentials or support for every model modality.

Provider communication belongs to the independent `lingxi-llm-client` Rust
library: chat, streaming, decisions, embeddings and image operations as enabled
by the selected provider. Provider-specific file/batch/retrieval APIs can have
different coverage. Realtime image/audio support is connector-specific. An SDK
method existing does not make it a tool available to the model.

Agent sessions, tool execution, permissions, hooks, compaction and orchestration
belong to Harness. Host/native credential services supply credentials; App
pages and generated Local App bundles must not embed or extract them. Bot
sessions use their own selected settings and grants. An auth failure should be
handled through the corresponding connection UI/flow, preserving account scope.

For SDK code, inspect the actual Cargo dependency and feature set. Use the
selected revision's `ChatRequest`, `RequestOptions`, response/stream types and
provider client documentation. Do not convert LingXi integration examples into
Anthropic SDK code because an old skill had a Claude name. Provider prices,
quotas and model catalogs are time-sensitive; this offline guide does not
maintain them. Use the account/provider source when the user needs current data.

Source anchors: llm-client `src/client/{mod,live}.rs`, `src/protocol`,
`src/providers`; Harness `crates/runtime/src/api.rs` and model/provider commands.
