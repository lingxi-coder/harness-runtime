# Prompt suggestions — Claude Code 2.1.296

Reference: npm `@anthropic-ai/claude-code/latest`, checked 2026-10-11, and the matching installed native executable. No paid provider requests were used for the comparison.

- Binary SHA-256: `c9b5341637becbd423ddffc5b254afb645682a3868cb708bbc6cc0e7bb419937`.
- Prompt: 1,377 UTF-8 bytes, SHA-256 `848ab6d7d37cb9e01ff58d81467438a21ff26e5287bbf510e76d387df592fb0d`.
- The oracle scripts extract and execute the shipped JavaScript functions, check the binary hash, and retain source offsets/hashes in fixtures. Ordinary extraction/VM execution is the evidence source; these are not inferred behaviors from documentation alone.

## Controls and lifecycle

The interactive and SDK paths have distinct enablement rules. The environment uses native tri-state, case-insensitive JavaScript whitespace parsing. Interactive initialization honors `tengu_chomp_inflection`, non-interactive/teammate gates and `promptSuggestionEnabled`. The native rollout default is false; LingXi uses an enabled fallback because it ships no remote rollout fetcher. The deterministic native control fixtures retain their original gate inputs. Explicit SDK opt-in bypasses the interactive rollout gate. Bare/simple and disabled-hook dispatch gates are independent of enablement. LingXi's attached mobile/desktop composers adapt the interactive surface without changing their provider query-source identity. Automatic SDK-surface generation requires an attached composer, and initialization waits for that attachment so an early transport turn cannot permanently disable the later UI. Headless SDK consumers retain their separate explicit opt-in path.

Generation checks cancellation, assistant-message count, actual API-error message identity, enablement, pending permissions, connected MCP elicitations, plan mode, rate limits and cache warmth in native order. Forced environment enablement permits `allowed_warning`, not a hard rate limit. `tengu_chomp_thaw` permits cache creation above the 10,000-token threshold; it does not permit uncached input plus output above that threshold. The shipped plugin-ownership predicate is a constant false. Automatic suggestions pass through real `prompt.suggest` Mod middleware before placement.

Only genuinely shown suggestions contribute to the unused streak. Twenty ignored submissions set the user-global streak to 20; after the 14-day new-user window, only every tenth eligible turn attempts generation. The one-time notice claims sentinel 21 and resets the eligible-turn position. Acceptance or a false-to-true config toggle resets the streak. Local commands participate in submission accounting without triggering a model query or prematurely displaying the back-off notice. Fresh persistent config homes initialize first-start metadata; custom/mobile homes do not read the ambient desktop user's config.

Typing cancels pending generation and hides an already shown preview; clearing the draft reveals it again. Escape does not have a special suggestion-dismiss action. Tab/right-arrow/click fills the draft; a subsequent edit still counts as accepted when the native acceptance timestamp rule applies. Turning the setting off affects future generation, while an already generated or in-flight suggestion remains valid. Unshown proposals that cannot be placed are discarded without increasing the unused streak. Session changes, new turns and publication share an ownership boundary so stale proposals cannot arrive after the next turn starts.

## Query and transport

The suggestion fork inherits the parent's model/provider, tools, system prefix, live thinking configuration, effort and ordinary output budget. It uses the main streaming policy with `query_source = prompt_suggestion`. `skipCacheWrite` leaves the trailing suggestion prompt unmarked and marks the preceding parent message. The native `tengu_basalt_spur` / `tengu_basalt_scarp` fork-pin branches and provider/beta/privacy gates are represented in the provider request policy.

The isolated no-tool fork retains streamed assistant text candidates in order, denies requested tools, and follows the native 50-turn bound. Selection first normalizes candidates until one is nonempty, then applies suppression once; a rejected first nonempty candidate does not fall through to a later candidate. The generation HTTP request ID is retained separately from the assistant message ID and the suggestion frame UUID.

The native frame is serialized in this exact field order:

```json
{"type":"prompt_suggestion","suggestion":"运行测试","uuid":"suggestion-1","session_id":"session-1"}
```

NDJSON adds one LF and uses native escaping for U+2028/U+2029. `generationRequestId` is private attribution and is not an extra frame field. `after_message_id` has been removed.

The SDK controller supports CLI and initialize opt-in, native reinitialize behavior, `set_prompt_suggestions_paused` validation and feature gating, synthetic-origin suppression, input cancellation, accepted/ignored feedback, and pending output while results are held. Pausing does not abort a request already started. The 30-second timeout starts at final input settlement; it is not a per-query background deadline. When live background tasks or unconsumed completion notifications hold a result, result frames are released before the retained suggestion.

Desktop, Android and iOS send separate **LingXi host extensions** for focus/draft context, actual display/acceptance and local command submission. A separate host state event carries the effective setting and transient back-off notice. These IPC extensions are not presented as native Claude SDK frames.

## Background sessions

The `CLAUDE_CODE_SESSION_KIND=bg` branch depends on the actual post-turn classifier result. A `blocked` result without a blocking operation permits generation without focus and without the unused-suggestion throttle. The resulting suggested reply is written only while the stored `needs` value still matches. Classifier and redactor helpers are retained as exact native JavaScript executed in the existing isolated QuickJS runtime; LLM work uses the shared provider stack. Background state storage is a LingXi session-storage adaptation, not a reproduction of Claude's daemon filesystem, timeline or job-management service.

## Verification record

The following completed checks are backed by executable fixtures:

- 521 native control/lifecycle cases: environment parsing, initialization, explicit opt-in, gate ordering, cache boundaries, back-off age/counters, notification/reset, presentation and focus.
- 137 normalization/filter cases, 9 candidate-order cases and exact prompt bytes.
- 42 native SDK state/serialization cases, plus live native no-provider pause-control captures.
- 25 native cache-marker cases and 64 provider/beta/privacy combinations.
- Production side-query suite: 5 tests passed, including real request serialization, request-ID attribution, live thinking switches, all streamed candidates, denied-tool continuation, the 50-turn cap, and exact classifier system blocks/string user content.
- Classifier route/selection suite: 3 passed against 278 native model-selection, denial, identity and managed allowlist cases.
- Production lifecycle suite: 11 passed, including raw UUID ownership, stale feedback, shown-only accounting, ignored-streak back-off/reset and in-flight setting behavior.
- Background suite: 4 passed, including 75 exact native helper functions in QuickJS, epoch/needs persistence, native input aggregation and actual classifier requests with live managed-model fallback.
- Shared runtime: `cargo check -p harness-runtime --features desktop,mobile` passed. Product `cargo check -p bridge-server` passed against the modified sibling repositories.
- Production SDK controller suite: 4 passed, including native pause frames, line escaping, final-settlement timeout and real task-registry hold/release ordering.
- SDK request-policy tests: 3 passed against the exact SDK library built for the Harness wire tests.
- Pure Rust control/filter suite: 7 passed. Pure SDK state suite: 3 passed. Client protocol suite: 2 passed, including exact field order and explicit host-extension round trips.
- Desktop: TypeScript checks, 121 focused tests and real Electron suggestion interaction passed.
- Android: `:app:compileDirectDebugKotlin --offline` passed with current generated protocol types.
- iOS: `LingxiCodeStore`, `StoreDebug`, arm64 iOS Simulator build passed with current generated protocol types; a SwiftUI expression was split to avoid compiler inference timeout.

Focused production suites pass. Broader pre-existing mobile unit-test fixtures still use obsolete message projections/options; those unrelated targets are not claimed passing. No paid-provider calls were needed for these controlled tests.

## Acceptance boundaries

Byte equality is verified for pinned prompts, deterministic native function outputs, controlled request policies and serialized SDK frames. Random identities, clocks and provider-generated text are supplied as controlled inputs in differential tests. This is not a claim that every network packet or the entire Claude Code application is identical: LingXi retains its authorized branding, multi-provider routing, native UI and storage adapters. General tool execution/validation/plugin hooks outside the isolated suggestion fork are not established by the no-tool request fixtures.

Rust checks use explicit local Cargo patches for the modified sibling repositories. Original dependency pins and lockfiles are restored after checks. Native UI compilation uses temporarily regenerated protocol bindings; existing ignored bindings are restored to remain paired with the installed native binaries. A release must build the new native libraries and bindings together. No signed package installation or physical-device acceptance is implied.

Public reference: [Claude Code prompt suggestions](https://code.claude.com/docs/en/interactive-mode#prompt-suggestions).
