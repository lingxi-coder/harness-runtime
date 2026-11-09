# Agent, reminder and Skill prompt baseline: Claude Code 2.1.319

The authority is the official darwin-arm64 executable, SHA-256
`1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd`.
`scripts/tests/fixtures/agent_reminders_319.json` records each extracted source
range, its byte offsets and digest. Its expected outputs are evaluated from
those actual JavaScript builders, with explicit imported host facts supplied to
an isolated VM. The application bootstrap and real provider are not executed.

| Surface | Native source / evidence | Production behavior |
| --- | --- | --- |
| General-purpose agent | `IRr` | Existing body preserved and independently checked. |
| Explore and Plan | `ORo`, `NRo` | Existing portable POSIX bodies checked; oracle also retains hybrid-search and PowerShell variants. |
| Web-reading agent | `NRr`, `SPt`, reporting constants | Updated same-site traversal, redirect trust, retry/failure and reporting instructions. |
| Child consent and notes | `QAn`, `HWi`, `NWi`, `FWi` | Both standard and `temporal_hare` rewrite variants are preserved. |
| Background session | `GCr`, `WWi` | Both text variants and all three isolation choices; durable output, conditional draft PR and subagent exceptions. |
| Initial/turn token budgets | `NAn`, baked model registry | Model-gated default; explicit configuration remains authoritative. |
| Silent-turn reminders | `W$n`, `q$n`, `fzr`, `pzr` | Model gate, count/time thresholds, transient reminder clocks, UTF-16 text and signed narration. |
| Narration classifier | `tst`, `fve`, `KPe`, `Uwn`, `VPe`, `act` | Decode signed protobuf fields 2 → 1 → 8; only non-empty narration counts as speaking. |
| Changed-file note | Native `edited_text_file` renderer | Empty snippets no longer claim that a shared snippet budget was exhausted. |
| Deferred-tool usage | Native `tool_search_usage_reminder` renderer | Existing text checked for empty, complete and truncated name inventories. |
| Shell output audience | Native `bash_output_audience_note` renderer | Existing text independently checked. |
| Task notification provenance | `DCe`, `Jdo` | Both ordinary and genuine-human-turn headers checked. |
| Skill tool | `rRr`, `o2i`, `Xwi` | Standard and rewrite variants; actual native rewrite selection, plus LingXi's existing reference-topic clarification. |

## Live child context placement

`scripts/tests/fixtures/agent_child_prefix_319.json` derives from unmodified
2.1.319 requests to a loopback mock API. Each source capture has a digest. It
contains three models: Sonnet 4.6, Opus 5 and Sonnet 5.5.

The child system body is agent instructions, consent, notes and, when enabled,
the initial total-token budget. Host environment, model identity and date are
three separate context sections in messages. Models accepting mid-conversation
system rows receive them after the task as one API system row. Other models
receive three system-reminder blocks before the task in its user message. A
fork preserves the inherited system prefix without re-appending these sections
when its resolved route and recorded prompt policy match the parent's. Missing
or stale policy metadata rebuilds the prefix. Explicit user system overrides,
append instructions and original role instructions cross this boundary as
separate UTF-16 sources; no renderer guesses their boundaries from prompt text.

The typed `SubagentPromptContext` keeps the system-only suffix separate from
message context. The default desktop renderer and the mobile guest-path
renderer use this same boundary.

## Child prompt routing

`agent::prompt_policy` resolves the child's admitted wire model and profile
through `SubagentApiClient::prompt_target`, the same ModelRuntime route used
for inference. The owning session's trusted fixed Chat/Code profile supplies
the mode. API protocol is not used as provider identity.

The system body, tool descriptions and nested tool context share the resolved
policy. Native Claude children retain the three captured prefixes. Portable
children receive their mode's Lean system template, the original role/custom
instructions and the bundle's consent/notes templates; their context does not
assert Claude identity or Native token-budget capabilities.

The child retains prompt sources and the admitted tool catalog. Explicit model
changes, refusal hops and admitted serving-model changes rebuild the next
request's system/context/tool descriptions. Unchanged retries reuse the same
policy. `turn.step` model/profile rewrites apply the same selection before
opening their physical request. Recovered children rebuild environment context
in the outgoing projection while retaining transcript history and custom
instructions. A changed-target fork rebuilds the child prefix; matching-target
forks retain the parent's exact UTF-16 prefix.

Silent-turn clock recovery aligns real JSONL timestamps with replayed history,
including merged assistant blocks sharing an inner ID and queued human source
IDs. Durable reminder payloads restore their history boundaries. Discarded
pre-compaction rows and missing timestamps cannot manufacture a current clock.

In-process teammates use the same policy and tool renderer, with a target-aware
host renderer for their full default system prompt. Their role addendum and
custom instructions remain present across model changes.

## Explicit normalizations and capability differences

- Product/file branding uses LingXi, LINGXI.md and the configured dot directory.
  Existing read-only agent instructions abstract Bash to the registered shell.
- Live capture comparisons normalize cwd, OS version and date. The captured
  private billing attestation and vendor SDK header are outside these body and
  context fixtures.
- LingXi WebFetch does not supply Native's `Fetched` approval/summary-pagination/
  stored-copy-date header. The web-reading agent applies the same upstream
  reporting limits to fetched content, describes ordinary truncation and omits
  the unsupported stored-copy-date claim. Traversal and retry limits are agent
  instructions and do not assert additional transport capabilities.
- The Skill reference-topic clarification remains an explicit product extension.
  Native coordinator-only Skill wording is retained in the oracle; LingXi's
  coordinator does not expose this executing Skill tool.

## Reproduction

```sh
node scripts/tests/agent_reminders_319_oracle.mjs /absolute/path/to/official/2.1.319 --check
python3 scripts/tests/agent_reminders_319_byte_check.py
python3 scripts/tests/agent_child_prefix_319_oracle.py /absolute/path/to/live-native-child --check
```

The byte check compiles the actual production background renderer and Skill
template selector with `rustc`. Rust unit tests additionally compare the real
child assembly, environment renderer, model budget defaults, timed history
scan, narration parser and individual reminder renderers against these
artifact-derived fixtures. These are scoped text/control checks, not model
quality or complete cross-provider wire acceptance.
