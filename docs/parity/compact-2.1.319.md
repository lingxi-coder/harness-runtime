# Compact and auto compact: Claude Code 2.1.319

Baseline checked on 2026-11-08 (America/Los_Angeles). The npm `latest` tag and
the installed official executable both resolve to 2.1.319. The executable is
`~/.local/share/claude/versions/2.1.319`, 321,995,200 bytes, SHA-256
`1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd`.
The official release manifest and the npm platform package's `package/claude`
member independently match those bytes; the npm tarball's SHA-512 integrity
also passed. The [npm `latest` tag](https://registry.npmjs.org/@anthropic-ai/claude-code/latest)
was checked again on 2026-11-09 and remains 2.1.319. The
[official release manifest](https://downloads.claude.ai/claude-code-releases/2.1.319/manifest.json)
provides the independent platform checksum.

## Evidence and reproducibility

The scripts verify that artifact's hash before evaluating functions extracted
from its original byte ranges. They do not treat minified function names as
stable identity across versions.

- `scripts/tests/compact_319_oracle.mjs`: six full prompts, eight partial
  prompts, eight formatting cases, five ordinary continuations, one exact
  UTF-16 continuation, and five response selections.
- `scripts/tests/compact_319_fallback_oracle.mjs`: 21 split/retry traces.
- `scripts/tests/compact_319_threshold_oracle.mjs`: 11 threshold/warning cases.
- `scripts/tests/compact_319_settings_oracle.mjs`: 49 command parser cases.
- `scripts/tests/compact_319_usage_oracle.mjs`: 15 usage-anchor cases.
- Each script's fixture is under `crates/compaction/tests/fixtures/` and can be
  checked again with `--check`.
- `scripts/tests/compact_live_oracle.py <binary> --expected-version 2.1.319
  --output <new-directory>` exercises the
  unchanged official executable against an isolated loopback mock provider.
  Eight scenarios passed: ordinary success, empty response, too few groups,
  no messages, whitespace response, provider overflow, multiple text blocks,
  and summary re-asking. It makes no real provider calls.

The task's artifact evidence, original function excerpts, REA bundle and live
results are in `../artifacts/compact-319-20261108/work/` relative to this
repository's root. `oracle-manifest.json` identifies their hashes and paths.
REA parsed the selected derived JavaScript ranges completely; its expanded
semantic graph has an explicit node-budget limit. The executable replay and
golden fixtures provide the behavioral assertions used here.

## Model selection across providers

Native's `syn -> iR -> WEr` fork takes an explicit cache-safe model override
when present, otherwise the current main-loop model. Its fallback summary
runner also inherits that model and the current thinking configuration. There
is no separate hard-coded Haiku or Sonnet role for ordinary compaction.

The ordinary summary request preserves the current **model, provider profile,
source model identity and thinking configuration**. For example,
an OpenAI main session compacts with that session's OpenAI route; a Gemini
session uses its Gemini route. The fork reads live thinking state instead of
retaining the mode captured when the session was constructed. Provider DTO
overrides remain explicit. Route projection failure stops before dispatch.

There is one separate Native recovery path: at the current model's blocking
limit, with no previous compaction failure or explicit override, `hxr` may
select the latest valid historical assistant's `requestedModel` when its
window is larger. `Zno` gives that candidate one physical attempt; failure
continues through the current model's ordinary compact path. It does not
select an arbitrary Sonnet/Opus tier. LingXi requires the historical candidate
to resolve on the **current provider profile** and pass the current model
policy, so an older conversation with another provider cannot silently route
its summary back to that provider. The attempt keeps the current sizing model
and contributes to the operation's shared physical-attempt budget.

The regression covers nine route/model pairs and three thinking modes each:
Anthropic, OpenAI, Gemini, DeepSeek, Grok, Qwen, Kimi, MiniMax and a custom
provider. This is routing/configuration coverage, not an account-level live
acceptance claim for all providers.

## Corrections found in this audit

1. **Native reserve and thresholds.** `AZr` is 20,000, not 32,000. The normal
   200k Native window gives 180k effective input, 167k auto compact, 147k
   warning and 177k hard blocking. A configured 100k window gives 80k input
   and a 67k auto threshold; it does not reduce the model's hard blocking
   ceiling. The 13k Native buffer remains exact even for that smaller window.
2. **Provider budgets.** Native Claude uses the verified Native window policy
   and 20k reserve. Other routes keep their published input limits, shared
   output reservation and selected context policy. A configured total window
   includes that route's actual output reservation. An unrecognized model name
   never replaces a known provider window with Claude's assumed 200k limit.
3. **Usage-based triggering.** The trigger uses the most recent valid response
   usage, its following message estimates, and Native's server-cleared-token
   compensation. Pure message estimation remains the fallback. Input, cache
   read, cache creation and output all count. Provider-native usage metadata
   with different field names retains canonical counters for cold restore.
   Compact boundaries prevent discarded usage anchors from resurfacing.
4. **Settings and commands.** `/autocompact` now persists and immediately
   applies the active model's window. `auto` is stored explicitly so it masks
   a global override. Higher source tiers, canonical/alias matching and env
   precedence follow the extracted settings resolver. Generic provider keys
   include the profile to isolate equal model names on different routes.
   Desktop reload, mobile settings edits and Side Chat snapshots carry the
   admitted settings without reading settings files on every model call.
5. **Disabled auto compact.** The setting and both disable environment gates
   suppress proactive and reactive automatic compaction. Explicit manual
   `/compact` remains available when only automatic compaction is disabled.
6. **Grouped automatic compaction.** A non-`auto` threshold source, including
   `model-default` and explicit window settings, uses the Native reactive
   grouping path. It preserves recent rounds and controls last-resort fallback
   separately from the full-history automatic path.
7. **Rapid-refill reporting.** The proactive breaker reports the candidate
   third rapid refill, rather than the previously committed count of two.
8. **Current model table.** Haiku 5.5 now has its verified Native 1M context,
   1M beta support and 128k default/upper output budget, including hosted names.
   Haiku/Sonnet/Opus 5.5 retain their distinct identities and `model-default`
   window source. Six isolated official-CLI resolver scenarios verify 1M
   defaults and the 200k fallback with the 1M disable flag.
9. **Historical larger-model recovery.** A blocked narrow model can use an
   admitted larger historical model once for the summary. Failed candidate
   attempts with known usage are charged independently of history commit;
   fallback retains the current model and the operation's remaining budget.
   Candidate admission includes server-cleared-token compensation, while the
   reactive last-resort decision keeps Native's separate live-context count.
   A host-only observation saves known usage before stream cancellation or
   non-stream response cleanup/telemetry can suspend; missing and invalid
   usage remains absent.

Settings reload follows the host's existing lifecycle. Ordinary Desktop hosts
watch the admitted settings files; Team hosts keep their existing disabled
watcher. Mobile applies `/autocompact` and `UpdateSettings` immediately, while
external file edits require rebuilding the host. Side Chat captures settings
at creation and restores that snapshot on recovery; later parent edits do not
propagate into an existing Side Chat.

The 20k threshold reserve is **not** the summary request's output cap. The
official live Sonnet 4.6 request uses `max_tokens=32000`; the summary fork
inherits the model's ordinary output budget. Other providers retain their
own admitted output limits.

## Alignment boundary

The tested default full prompt, continuation, summary selection and 21 retry
traces are unchanged from 2.1.318. The fixtures additionally cover partial
prompts, settings parsing, usage anchoring and pressure thresholds.

The existing project choices remain intentional:

- Append `COMPACT_SUMMARY_PROVENANCE_GUIDANCE` after the byte-locked Native
  core prompt and omit only host-origin `skill_activation` instruction rows
  from the summarizer's view.
- Share five physical API attempts per normal operation; idle compaction uses
  its existing single-attempt budget. If no valid summary is produced, fail
  without replacing the original history.
- Preserve provider-specific request protocols, actual route limits and
  project-specific attachments. Complete HTTP request bytes and generated
  summaries are not identical across LLM providers.

Background precompute is gated by `tengu_sepia_moth` (default false) in the
official 2.1.319 artifact. LingXi's existing idle scheduler is separate from
that optional service-driven feature. Optional time-based microcompact, snip,
classifier and precompute branches are not claimed as fully equivalent by
these default-path checks.

## Validation

The final checks run on 2026-11-09 are logged under
`../artifacts/compact-319-20261108/work/final-checks/` relative to the repository
root. The command and exit code for each group are saved alongside its log.

| Check | Result |
| --- | --- |
| Compaction crate, including prompt/context-policy integrations | 321 passed |
| Core settings and request-provenance contracts | 6 passed |
| `/autocompact` command and persistence | 13 passed |
| LLM runtime windows, physical budgets and usage observation | 21 passed |
| Provider forks, live thinking, SSE and non-stream compaction | 18 passed |
| Orchestrator compaction, context usage and turn recovery | 156 passed |
| Persistence, force-compact, proactive and hook integrations | 36 passed |
| Runtime Desktop + Mobile feature check | Passed on the macOS host |

All 571 selected Rust tests passed. The feature check compiles both host
composition paths; it does not constitute packaged-app or real-device
acceptance.

The five artifact-hash-checked JavaScript oracles also pass `--check`; the
unmodified official CLI passed eight mock-provider scenarios and six model
window resolver scenarios. No real provider-model calls are represented by
these local checks.

An initial orchestrator build exhausted local disk while writing incremental
caches. Inactive caches were released, and the orchestrator checks use the
command-line `profile.test.package.orchestrator.incremental=false` override;
the repository's Cargo profile settings are unchanged.

Publication was additionally verified in an isolated worktree using main's
existing SDK revision `f16f3d5da69de7067f1b60f3b3fdbfb1c7323d8f` and only this
patch. The locked Desktop + Mobile check, 223 focused tests (156 orchestrator,
36 lifecycle, 18 provider forks, 13 command), and all five byte-oracle checks
passed there. This verification excludes unrelated checkout changes.
