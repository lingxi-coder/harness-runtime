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

In a session, `/model` lists selectable models. `--model` sets the model for
that launch. The choice is saved as `model` in the user `settings.json`, and
up to eight recent choices are kept in `recentModels`, newest first. If the
config directory cannot be found, the choice stays in the current session. A
managed allowlist can hide models; a normal install has none, so the picker
shows what the configured providers offer.

Read the accounts topic for host-global multi-account selection. A model name
alone is not a usable connection. Logical
provider profile, account ID and execution binding are different identities.
A request can select another healthy allowed account at execution/retry time;
do not infer its route from a display name. Desktop model selection is native
state; editing a Harness default does not update an existing session's picker.
Local ToolSearch discovers permitted tools without changing model providers.
Selected schemas are ordinary functions on generic routes; Anthropic-native
references/defer_loading apply only to the effective Anthropic wire route.
SDK support for a provider's hosted search does not mean this host exposes it.
