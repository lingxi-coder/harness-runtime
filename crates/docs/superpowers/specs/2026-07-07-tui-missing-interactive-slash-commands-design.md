# Design: complete the TUI's missing interactive slash commands

**Date:** 2026-07-07
**Status:** Draft — awaiting review
**Scope:** Wire every *feasible & applicable* claude-code slash command that `command_core` already knows about but the ratatui TUI palette never surfaced. User decision: "everything feasible"; **skip** `/login`, `/logout`, `/privacy-settings`.

## 1. Problem

The TUI palette (`tui/src/command.rs` `BUILTIN`, ~45 advertised) wired the prompt-injector, read-only-report, and orchestrator commands, but skipped the **interactive-dialog** commands whose real body must live in the TUI. `command_core` registers all 101 builtin names (`command-api/src/builtin_support/names.rs`) and routes these to a fallback stub (`register_interactive_only_commands`, `HOST_BOUND_DEFERRED_GAPS`) that literally tells the user to "use interactive TUI mode" — but the TUI never built the views. `/btw`'s own deferral note reads *"deferred for lack of TUI dialog infra"* — which the `/permissions` editor + `/resume` picker since built. This closes those gaps.

## 2. Authoritative classification (from `names.rs`)

`is_palette_hidden` (26 names) + `ENV_DISABLE_GATED` (5, default-on) define what claude-code shows. Everything else in `BUILTIN_COMMAND_NAMES` (101) should be palette-visible. The TUI surfaces ~45; the rest are the gap. Split into BUILD (feasible+applicable to LingXi) vs SKIP (intentional divergence / platform / removed-upstream / claude-code-faithful-stub).

### 2.1 BUILD — feasible & applicable (13)

Each gets **real behavior** (project rule: never advertise "not implemented"). Grouped by effort.

**Tier 1 — self-contained (small view/toggle + a small seam):**
| cmd | type | behavior | seam |
|---|---|---|---|
| `/fast` | toggle | Toggle fast mode `[on\|off]` (LingXi has fast mode: `run.rs` `fast_mode_state`/`supports_fast_mode`) | session fast-mode flag → model request |
| `/terminal-setup` | action | Install Shift+Enter (Apple Terminal: Option+Enter) newline binding; **isHidden** on Ghostty/Kitty/iTerm2/WezTerm (native CSI-u) | writes terminal config file |
| `/rename` | dialog/arg | Set a custom session title (arg, or auto-generate from convo via a recap-style call) | session-store custom-title write |
| `/add-dir` | dialog/arg | Add a working directory to the session | reuse `permission::persist_workspace_directory` + orchestrator dir set |
| `/diff` | view | View uncommitted git changes / per-turn diffs (reuse `tui_core::render::diff`) | shell `git diff` (read-only) |
| `/sandbox` (`sandbox-toggle`) | toggle | Toggle sandbox mode for bash commands | session sandbox flag |
| `/btw` | dialog | Ask a quick side question without interrupting the main conversation (a one-off prompt on a side channel) | orchestrator side-turn |

**Tier 2 — medium:**
| cmd | behavior | note |
|---|---|---|
| `/branch` | Branch the conversation at this point into a new session | builds on `fork_conversation` + resume re-mount seams |
| `/tasks` | List/manage background bash tasks | **requires** wiring a bg-task feed into `run_app` (today dropped for "no feed reaches run_app"); defer if the feed is absent |

**Tier 3 — large (own design; §4):**
| cmd | behavior |
|---|---|
| `/plan` | Enable plan mode or view the current session plan |
| `/rewind` (aliases `checkpoint`/`undo`) | Restore code and/or conversation to a previous point — needs checkpointing infra |
| `/plugin` (+`/reload-plugins`) | Manage plugins (marketplace UI) + activate pending changes |

### 2.2 SKIP — intentional divergence / platform / removed (documented)

- **Multi-provider divergence → `/connect`:** `/login`, `/logout` (user-confirmed skip). LingXi is multi-provider; `/connect` owns auth. See [[lingxi-accepted-divergences]].
- **User-confirmed skip:** `/privacy-settings` (consumer-subscriber-gated; opens external settings).
- **Platform / Anthropic-product divergences:** `/mobile` (LingXi has its own mobile), `/desktop`, `/session`, `/remote-env`, `/remote-setup`, `/bridge` (remote-control), `/chrome`, `/ide`, `/install`, `/install-github-app`, `/install-slack-app`, `/voice`, `/upgrade` (Max upsell), `/passes` (referral), `/feedback` (no LingXi endpoint), `/x402` (crypto).
- **Removed upstream (0 objects in v2.1.183 binary):** `/vim`, `/pr-comments`, `/output-style` (`/vim` currently still in the TUI — a pre-existing mild divergence, out of scope here).
- **`CORRECT_BY_DESIGN_STUBS` (23) + `HIDDEN_PALETTE_COMMANDS` (3):** claude-code itself disables/hides/ships-as-`name:'stub'` → faithful non-goals, keep on the stub handler.

## 3. Architecture

Each BUILD command follows the **established TUI slash pattern** (proven by `/permissions`, `/resume`, `/connect`):

- **Palette:** a `SlashCommand` row in `tui/src/command.rs` `BUILTIN` + a `cmd_*` dispatch method on `ChatWidget`. `/terminal-setup` uses claude-code's `isHidden` gate (native-CSI-u terminal detection) so it is registered but `advertised: false` when the current terminal is Ghostty/Kitty/iTerm2/WezTerm.
- **Read-only / static** commands (e.g. `/diff` render, `/fast`/`/sandbox` toggles that only flip a local flag) run inline via the `block_on` read bridge.
- **Interactive dialogs** (`/rename`, `/add-dir`, `/btw`, `/branch`, `/plan`, `/plugin`, `/rewind`) get a `bottom_pane/*_view.rs` view (like `permissions_editor_view.rs`) + a `ViewOutcome → BottomPaneOutcome → ChatOutcome → AppCallbacks → mode.rs` effect closure for any engine side-effect (the off-loop `/web` pattern — network/state mutations never `block_on` on the sync render thread).
- **Engine side-effects** (`/rename` title write, `/add-dir` dir set, `/branch` fork, `/rewind` restore) go off-loop via a new `ChatOutcome` variant handled in `apps/cli/src/mode.rs`, reusing `command_core` handlers where they already exist and adding orchestrator seams where they don't.
- **Reuse `command_core` handlers** through `run_core_command` where a real handle-free/handle-bound handler already exists; the TUI's interactive view replaces the "interactive TUI mode" stub for the `HOST_BOUND_DEFERRED_GAPS` / `interactive_only` names.

## 4. Big-three design (Tier 3)

### 4.1 `/plan` — plan mode
LingXi already has plan-mode primitives: the `EnterPlanMode`/`ExitPlanMode` tools, `RenderedMessage::{UserPlan, PlanApproval}`, and a plan-approval cell. `/plan` (a) with no active plan → **enable plan mode** (set the session into plan mode so the model plans before acting, mirroring `EnterPlanMode`), (b) with an active plan → **view the current session plan**. Design: a `PlanModeState` on the session + a bottom-pane view showing the current plan; enabling flips the mode flag the turn-loop reads. Verify against claude-code `commands/plan/` + the plan-mode turn gating. **Needs a design confirmation before build.**

### 4.2 `/rewind` — restore to a checkpoint
The largest: claude-code checkpoints (code snapshots + conversation position) and restores either/both. LingXi has **no checkpointing infra** — this needs: a per-turn checkpoint store (git-stash-like code snapshot + conversation-message index), a picker view listing checkpoints, and a restore path (reset working tree + truncate/rewind the transcript & JSONL). This is effectively its own feature. **Design + scope confirmation required before build; likely a separate PR.**

### 4.3 `/plugin` (+`/reload-plugins`) — plugin management
LingXi has a plugin CLI (per project history) but no interactive marketplace UI. `/plugin` = a bottom-pane view listing installed/available plugins with enable/disable/install/marketplace actions; `/reload-plugins` = activate pending changes in the live session (re-walk plugin dirs, refresh the shared registry — analogous to `/reload-skills`). Verify against `commands/plugin/` + `reload-plugins/`. **Design confirmation before build.**

## 5. Phasing

1. **Phase 1 (Tier 1, ~7 cmds):** fast, terminal-setup, sandbox, diff, rename, add-dir, btw. Each: palette row + view/toggle + seam + tests. Commit per small batch.
2. **Phase 2 (Tier 2):** branch; tasks only if the bg-task feed is wireable into `run_app` (else keep deferred + note).
3. **Phase 3 (Tier 3):** plan → plugin/reload-plugins → rewind, **each preceded by a design confirmation** (they add real subsystems). rewind likely its own PR.
4. Update `command.rs` `BUILTIN`, the `advertised()`/help/completion single-source test, and the `names.rs` classification (move built names out of the deferred/interactive-only buckets).

## 6. Testing

- Per command: palette resolution + advertised/help single-source test (existing `registry_is_the_single_source_for_completion_help_and_dispatch` extends automatically), view unit tests (key handling, render), and the effect-closure/seam unit test.
- `/terminal-setup` isHidden gate: advertised only on non-native-CSI-u terminals (unit-test both branches).
- `cargo test -p tui -p command-core -p command-api` + workspace `cargo check --tests` green; real iTerm2 smoke for the interactive views (TestBackend emits no ANSI).
- Keep the `names.rs` partition tests green (update counts as names move from stub → implemented).

## 7. Out of scope

- The `/vim` removed-upstream cleanup (pre-existing divergence).
- Re-wiring `/tasks` if no bg-task feed exists in the TUI path (documented deferral).
- Full checkpointing infra beyond what `/rewind` minimally needs (its own project if it balloons).
