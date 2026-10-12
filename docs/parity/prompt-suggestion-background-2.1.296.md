# Background suggestion admission — Claude Code 2.1.296

Reference artifact: local native Claude Code 2.1.296, SHA-256
`c9b5341637becbd423ddffc5b254afb645682a3868cb708bbc6cc0e7bb419937`.
This document covers the background branch of `Y$o`, not the complete daemon.

## Evidence and reused code

`scripts/tests/prompt_suggestion_background_296_oracle.mjs` reads the native
artifact, checks its hash, and checks 75 retained function bodies against its
actual source bytes. It also evaluates the original classifier prompt module:
classifier system text is 16,877 bytes; the optional leading-ask block is 1,562
bytes. `crates/orchestrator/tests/fixtures/prompt_suggestion_background_2_1_296_sources.json`
records each source function's binary byte offset, byte length and SHA-256.

The retained functions implement classifier preclassification, heuristic fallback,
JSON response normalization, input text aggregation, stable tool-count ordering,
Unicode truncation, context redaction and credential redaction. Native imports are
isolated in separate scopes; only schema/JSON/library boundaries are adapted.
`hooks::function_hook::Sandbox` executes the same guest program in production.
No JavaScript guest performs provider I/O.

## Wired behavior

For `CLAUDE_CODE_SESSION_KIND=bg`, post-turn processing runs the classifier before
suggestion focus admission. A classified `tempo=blocked` result without a
structured `block` allows the native background focus exception and skips the
ordinary unused-suggestion cadence. The input UI cannot invent this state.

The pipeline honors `tengu_classifier_disabled_surfaces` (`bg`),
`tengu_cobalt_wren`, `tengu_bg_classifier_config` (`useSmallFastModel`,
`disableThinking`) and `tengu_shimmering_firefly`. Preclassification can avoid a
provider call. Otherwise the same SideQueryClient/llm-client path sends the native
system blocks and exact retry text; two parse attempts precede the native
heuristic fallback. The request layer resolves cache scope/TTL from the live
route/account context. The adaptive-thinking model exception reserves the native
2,048 extra output tokens.

Text aggregation uses the newest assistant window, excludes API-error text, and
retains the full assistant history for tool counts. Ties preserve insertion order.
The three excluded tool names are `ScheduleWakeup`, `CronCreate`, and `Monitor`.
The host projects ordinary single-text user inputs into the latest-ask field;
structured/multimodal inputs are not guessed into a plain-string native input.

At placement, the original redactor must return the suggestion unchanged.
Persistence additionally requires unchanged `needs`, a still-blocked stored state,
and a live session epoch. Staged writes validate both epoch and the currently
stored JSON before a synchronous atomic rename under the lifecycle lock. A new
turn/session reset cannot publish an older generation after changing that epoch.
Session identifiers at this boundary are raw UUID strings.

## Verification and boundaries

The native-source oracle passed. The production QuickJS and wired background test
suite passed in the parent-run log `/tmp/lingxi-suggestion-orchestrator-final.log`.
The final parent run passed all four background tests, including the corrected
module namespace/raw-UUID path and the extended model-selection pipeline.
Focused tests cover exact text aggregation/prompt bytes, native JSON parsing,
secret redaction, no-text input detection, reset/needs guards, an unfocused
background pipeline that persists its real generated reply, and an external
`needs` change which prevents that reply from being persisted. Providers are
synthetic; no live paid model request is part of these tests.

The product has no Claude daemon job store or chat-relay tool registry. The
suggestion-facing state lives at
`<config_home>/prompt-suggestions/<raw-session-uuid>.json`; this is an explicit
host storage adaptation, not a claim that the original daemon's job files,
timeline, notification bridge, names, links or full record are reproduced.
Claude's chat-relay emitted-tool-text/recap-card branch is not synthesized from an
unrelated UI flag. The 75 function bodies and prompt texts have byte evidence;
that evidence does not establish whole-application or complete daemon byte parity.

## Native small-fast model selection

The classifier now uses `llm-runtime::model::classifier_model_policy` for
`Pw` / `Glt` / `_v` instead of inferring a provider from its model name. The
orchestrator feeds the actual selected route, redacted credential facts,
profile-scoped catalog defaults, explicit/default-env model attribution and live
managed settings. Only SDK/runtime credential owners inspect native API-key
shape; no secret is returned to the orchestrator or UI.

The policy covers first-party endpoint/API-key availability, Anthropic AWS/Google
Cloud, gateway Haiku availability, Bedrock/Vertex Sonnet fallback, presence-sensitive
small-fast/default-Haiku overrides, denied-model fallback, and the exact managed
allowlist exception for noncanonical overrides. Model and profile resolve
together. Other product providers retain their configured model.

`scripts/tests/prompt_suggestion_model_296_oracle.mjs --check` executes the original
native selector/parser/matcher functions to verify 136 selection, 81 denied-model,
49 exact-allowlist and 12 canonical-identity cases. The checked-in fixture lives
under `crates/llm-runtime/tests/fixtures/`. The production Rust policy and route-fact
tests passed in the parent run. The background pipeline test additionally
captures actual classifier requests: it selects the configured Haiku model,
then changes live `deniedModels` to force the current main model without changing
provider profile. It checks native system blocks and the adaptive-thinking
output-token reserve. The extended pipeline passed in the parent's final suite.
