# lingxi-core

Pure-function conversation state machine for the LingXi engine. Provides:
- `events::Event` — inputs to the reducer (user messages, API stream events, lifecycle).
- `state_machine::ConversationState` — the state set (Idle, AssemblingPrompt, AwaitingApiResponse, StreamingResponse, Terminated).
- `reducer::reduce(state, event) -> (state, effects)` — pure-function state transitions.
- `prompt::assemble_request` — build the Anthropic-shape API request body.
- `session::SessionState` — persisted session model.

No I/O, no `tokio::spawn`. All side effects are returned as `lingxi_protocol::Effect` values for the host's `EffectHandler` to process.

See `docs/superpowers/specs/2026-05-22-lingxi-core-rust-engine-design.md` §5 (D2 Engine layer).
