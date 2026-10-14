# `/model` source and selection API map

Read-only review of the Harness checkout at `cbb720052be8bf80370e6af70a972852d03a59bf`.
No production source was changed and no Cargo command was run. File SHA-256 values
below identify the source snapshot inspected; this worktree contains unrelated
parallel edits.

## Current request paths

H has no `NativeModelControl` or `set_user_model_selection` symbol. The concrete
selection API is `OrchestratorHandle::switch_model_with_source(model: &str,
profile: Option<&str>, source: &str)`, implemented by the orchestrator's
`switch_model_with_hook_source`.

The primary slash-command path starts with `ClientCommand::RunSlashCommand.raw`
as `ClientTextDto`. That DTO stores exact UTF-16 `code_units` and serializes them
as a normal JSON string. `parse_slash_command` converts it to a
`Utf16JsonProjection` with `raw_args` and positional-token projections. The
desktop/general `ModelHandler` immediately calls `raw_args_utf8()`; this returns
`None` when the exact units are not representable as the same Rust UTF-8 string,
and the handler stops with “Model argument must be valid Unicode text.” For
ordinary text, it trims, calls `parse_model_ref`, and passes the resolved String
model/profile to `switch_model_with_source(..., "command")`.

Mobile has a separate builtin `/model` fast path in `execute_claimed_slash`: it
decodes `raw_args.string_units("")` with `String::from_utf16`, then trims and
resolves a route before calling `switch_model_and_remember`. If decoding fails,
the fast path is not selected and the request falls through to the normal
dispatcher/handler. `ClientCommand::SetModel` is a distinct API with `model:
String`; it resolves a routable model, then calls the same switch helper with
source `"sdk"`. Interactive mobile selections persist the *qualified resolved
route* in `last-model.json`; startup reloads it into `runtime.model` and
`runtime.model_profile`.

Headless stream-json `control_request` subtype `set_model` parses the frame as
`Utf16JsonProjection`, checks the `model` JSON value, treats absent/null or a
trimmed case-insensitive `default` as the default sentinel, and otherwise passes
the string to `switch_model_with_source(..., "sdk")`. The route has no
provider-profile input. It is separate from slash `/model`, although both end at
the same orchestrator switch implementation.

## Selection ownership and consumers

`SessionModelSettingSource::{Configured, UserSelection}` already distinguishes
ownership. A non-default `command`, `picker`, or `sdk` switch writes the selected
model to `SessionState.model`, the resolved provider to `model_profile`, and the
`model` argument received by the switch API to
`user_model_setting: Option<String>`. That value is not guaranteed to be the
original slash-command tail: `/model` has already run `parse_model_ref`, and
Mobile `SetModel` has already resolved the routable model. It sets provenance
to `UserOrEnv` and source to `UserSelection`; both the same-route and changed-
route branches do this. Selecting `default` clears the raw value and restores
`Configured`. Automatic/resume switches do not overwrite a prior user
selection.

This user-selection value is not an exact UTF-16 source carrier. It is a Rust
String containing the switch API's model argument. By contrast, the configured parent value has a separate carrier:
`ModelRuntime.configured_model_source: Arc<RwLock<Option<Utf16JsonProjection>>>`.
`with_main_model_selection` and `refresh_main_model_selection` update that
configured projection. `configured_model_source_for_request` only returns it
when the current session owner is `Configured` and the request/session route
matches; a `UserSelection` deliberately stops inheriting the configured
parent's source. Thus there is already an ownership distinction, but there is
no equivalent source projection for the user selection itself.

The current raw String's direct semantic consumer is
`effective_main_loop_selection_for`, which passes it to the agent's main-loop
model resolver. Other current consumers inspect the `SessionModelSettingSource`
to preserve user-over-configured precedence and configured-source inheritance.
The configured-source projection is separately consumed by request preparation
and Insights model selection; it must not be repurposed to carry a user pick.

## Persistence and restore

`SessionState` is serde-serializable and contains resolved model/profile plus
`user_model_setting: Option<String>`, provenance, and ownership enum. In the
current transcript path, assistant JSONL rows persist the resolved model and
`modelProfile`. Cold replay rebuilds those resolved fields from assistant rows;
`ReplayedSession::handle_runtime_snapshot` explicitly supplies `None` for
`user_model_setting`, provenance, and source. Therefore the cold JSONL resume
path does not restore the raw UserSelection. Hot/in-place resume can carry the
same raw String and ownership through `ResumeRuntimeSnapshot`, and
`resume_session` restores them when present. Mobile's `last-model.json` is a
separate preference for an interactive resolved model/profile; it is not the
UserSelection source projection.

## Smallest source-preserving interface change

Reuse the existing `Utf16JsonProjection` already present at slash ingress. The
resolved route (`model` + `profile`) and the exact user-selected source value
must travel separately: route resolution may need valid Rust text, while the
selection source must retain original UTF-16 units. Update the current switch
API and all callers to carry that source explicitly; do not overload
`configured_model_source` or infer source from route equality.

The current UserSelection raw-value slot should become a root-string
`Utf16JsonProjection` (paired with the existing `SessionModelSettingSource`),
and the same typed value should be carried by `ResumeRuntimeSnapshot` for
hot/in-place resume. Update the Plan-mode consumer to read the projection
without lossy conversion. For cold resume, the current JSONL model/profile
fields only preserve the resolved route; whether a UserSelection raw value must
be added to durable transcript state depends on the Native source contract and
is intentionally left to that source review.

The caller set for that migration is bounded to:

- slash `ModelHandler` and Mobile's `/model` fast path;
- `OrchestratorHandle` and `switch_model_with_hook_source`;
- `SessionState` and hot `ResumeRuntimeSnapshot` restore;
- configured initialization/refresh in `with_main_model_selection` and
  `refresh_main_model_selection` (keep their configured source projection
  separate);
- Plan-mode raw-setting consumer `effective_main_loop_selection_for`;
- headless `set_model` control request;
- mobile `ClientCommand::SetModel` and its persisted resolved-route preference
  only if the Native source contract requires retaining the original pick.

This map does not infer Native UI state or decide which of these inputs Native
stores; that remains with the Native source owner.

## Source snapshot hashes

| File | SHA-256 |
| --- | --- |
| `crates/command-api/src/parser.rs` | `4f3d10c87dd5a20f847fdaec84897d067aa8aef87e0eaa017d0d4ba680494dda` |
| `crates/command-api/src/builtins/model.rs` | `b5dd5f9d66cb0000b717e0db82648fc8ff8082387a759ba2d1744a2369ef33b4` |
| `crates/client/src/protocol/text.rs` | `b7e5f9e2e76940ae6e3c51d742ff3e40ca6b608c4a4fd1660209c6d3b75b73db` |
| `crates/client/src/protocol/commands.rs` | `6cee68814bbf47cbc044f7865cc9d83a27fff2145ca2271ec51d43e4303d1eb7` |
| `crates/core/src/session.rs` | `82098dc639c47c920aa58f40c5521bc926cbf535df0ce09718846898336267fe` |
| `crates/core/src/host/orchestrator.rs` | `71498a71c52c553f4665b0af3814d28c59033835c1356bc103db0af66b916ea2` |
| `crates/orchestrator/src/handle_impl.rs` | `80a89f9daa97ff7d2de37c4f7099aa50c40f0cb7f1422d71fc2f42c18dc1a7fa` |
| `crates/orchestrator/src/conversation/wiring.rs` | `f681eefc4d8cfc932178571398d73e664f2c15562deeed2554bb1a4cd74a2c25` |
| `crates/orchestrator/src/conversation/model.rs` | `7eeb82331dca4d76195a6bd19de2eca21cc41cf1e09d8433be85a9b7462acc17` |
| `crates/orchestrator/src/conversation/compaction.rs` | `c6dbd061f071aa384bff194ba4d2960da5f549fbca5a5ee6ae583196c84ce2b4` |
| `crates/orchestrator/src/resume.rs` | `ffcdc33485040f28c82fcbc185660adb96718993c5cc4ade4018a87a63d20929` |
| `crates/orchestrator/src/conversation/transcript.rs` | `bab0adaaa8454870da04799cbe9768d9ff9cfa016d708d53b62255e5b520359c` |
| `crates/runtime/src/headless/run/control.rs` | `0bdc38977bb3a716ee5ed6920aa553ea6b429f0d49c050b73772cedb45715833` |
| `crates/runtime/src/mobile/host.rs` | `4dd7e0a92ff5bdb6fdc3139efc986074c0d662b76a53c6ad343ff14d48effda7` |
| `crates/runtime/src/mobile/host/model_preference.rs` | `f6416126165b018c6d2c01ff8f23a78bf7cf6aa889f59b2fe63ac2629fe191e1` |
