# /web + /connect ratatui TUI re-port — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Re-port the deleted iocraft `/web` (WebSearch config) and `/connect` (provider OAuth) slash-command subsystems into the active ratatui `tui` crate.

**Architecture:** The old iocraft TUI used pure `KeyCode→Outcome` reducer screens + `pending_*` flags on `Arc<Mutex<AppState>>` drained by async ticker pumps that reached back into the live screen. The ratatui `tui` crate is self-contained views (`BottomPaneView`) whose effects flow OUT via `ViewOutcome→BottomPaneOutcome→ChatOutcome→AppCallbacks` closures (which own a tokio `Handle` + runtime deps and `.spawn()` async work). Async RESULTS flow back IN via channels the `RataApp::run` loop drains (`events_rx`/`permission_rx` today). We reuse the pure reducers verbatim, wrap them in ratatui views, and adapt feedback to ratatui's transcript-oriented model: results are pushed as a new `TurnEvent::SystemNotice` (append-only) via the existing `turn_tx`; the web snapshot is a shared `Arc<Mutex<WebConfigSnapshot>>`.

**Tech Stack:** Rust, ratatui 0.29 / crossterm 0.28, tokio. Reused crates (already in tree): `tool-web` (`tool_web::web_search_config`, `tool_web::web_search_client`), `command-core` (`command_core::{OAuthConnectDriver,CopilotConnectDriver}`), `secret` (`CredentialManager`), `memory` (`lingxi_md::user_config_dir`).

## Global Constraints

- Repo has NO git remote — commit locally only, no PR.
- NEVER run `cargo fmt` on the workspace.
- After any render change run BOTH `cargo test -p tui` AND `cargo test -p test-harness`.
- Deleted iocraft source (verbatim reducer bodies) lives at git ref `f4ddad16f`, path prefix `lingxi-code/tui/src/` — read via `git show f4ddad16f:lingxi-code/tui/src/<path>`.
- Registry principle (`tui/src/command.rs`): NEVER advertise a command without a real data source on this backend. Both `/web` and `/connect` now have real data sources, so they get registered.
- Provider auth tags are exactly `"api_key"` | `"copilot_device"` | `"oauth"`. Web credential ids are literally `"web:tavily"` / `"web:brave"`. Connect provider keys use the raw `provider_id`. All persisted via `CredentialManager::set_provider_key(id, secret)`.
- Settings file: `~/.lingxi/settings.json`, nested `{ "webSearch": { "provider", "searxngUrl" } }`, read-modify-write preserving other keys, pretty-printed + trailing newline (`tool_web::web_search_config::WebSearchConfig::{from_settings_json,write_settings_json}`).

---

## FEATURE 1: /web (do first — self-contained, headlessly testable)

### Task 1: deps + port pure reducer modules verbatim

**Files:**
- Modify: `tui/Cargo.toml` (add deps)
- Create: `tui/src/web/mod.rs`, `tui/src/web/picker.rs`, `tui/src/web/config.rs`
- Modify: `tui/src/lib.rs` (add `mod web;` — or `pub(crate) mod web;`)

**Steps:**
- Add to `tui/Cargo.toml` `[dependencies]`: `tool-web = { path = "../tools/web" }`, `secret = { path = "../secret" }`, `memory = { path = "../memory" }`, `command-core = { path = "../commands/core" }`, `url = { workspace = true }`, `dirs = { workspace = true }`. (Verify each `{ workspace = true }` external is declared in the root `Cargo.toml [workspace.dependencies]`; if `url`/`dirs` are not, pin the same version other crates use — check `tools/web/Cargo.toml` and `apps/cli/Cargo.toml`.)
- `git show f4ddad16f:lingxi-code/tui/src/screens/web_picker.rs` → `tui/src/web/picker.rs`. Port VERBATIM. It already imports `tool_web::web_search_config::WebSearchProvider` and takes `crossterm::event::KeyCode`. Keep every type, fn, and the render oracle `render_web_picker_to_string`. Copy its `#[cfg(test)]` tests too.
- `git show f4ddad16f:lingxi-code/tui/src/screens/web_config.rs` → `tui/src/web/config.rs`. Port VERBATIM; fix the `use crate::screens::web_picker::...` import to `use crate::web::picker::...`. Keep `render_web_config_to_string`, `is_http_url` (uses `url` crate), tests.
- `tui/src/web/mod.rs`: `pub mod picker; pub mod config;`
- Add `mod web;` to `tui/src/lib.rs`.
- Verify: `cargo test -p tui web::` compiles + the ported reducer tests pass.

### Task 2: WebPickerView + WebConfigView (ratatui views)

**Files:**
- Create: `tui/src/bottom_pane/web_picker_view.rs`, `tui/src/bottom_pane/web_config_view.rs`
- Modify: `tui/src/bottom_pane/mod.rs` (register the modules + a `show_web_picker`)

**Interfaces produced:**
- `ViewOutcome::RunWebAction(WebAction)` (Task 3 adds the enum; this task can stub by returning `Pending` and be completed after Task 3, OR define `WebAction` here — define it in `view.rs` in Task 3 and have views reference it). To avoid ordering issues, DO Task 3's enum plumbing FIRST if needed; the reviewer will treat the two as one unit.

**Design:**
- `WebPickerView { state: crate::web::picker::WebPickerState }`. `impl Renderable` mirrors `model_picker_view.rs`: modal via `centered_rect`+`Clear`, render lines from `render_web_picker_to_string` (split on `\n`) OR re-derive with proper theming — prefer building `Line`s directly (header "Configure web search", search row, per-visible-row marker `❯`/`✓`/spaces + `label` padded to 12 + description, footer). `desired_height` = visible rows + chrome (~6).
- `handle_key(KeyEvent)`: call `crate::web::picker::handle_web_picker_key(&mut self.state, key.code)` and map `WebPickerOutcome`:
  - `Select(provider)` → `ViewOutcome::OpenView(Box::new(WebConfigView::new(provider, self.state.snapshot.clone())))`
  - `Test(provider)` → `ViewOutcome::RunWebAction(WebAction::TestSearch { provider, typed_key: None })`
  - `Cancel` → `ViewOutcome::Cancelled`
  - `Stay` → `ViewOutcome::Pending`
- `WebConfigView { state: crate::web::config::WebConfigState }`. `new(provider, snapshot) = WebConfigState::new(provider, snapshot)`. Render mirrors `render_web_config_to_string` (title, status_text, masked `Input: ***` for Tavily/Brave, raw for Searxng, footer). `handle_key`: `handle_web_config_key(&mut self.state, key.code)` → map `WebConfigOutcome`:
  - `SaveSecret{provider,secret}` → `RunWebAction(WebAction::SaveSecret{provider,secret})`
  - `SaveSettings{provider,searxng_url}` → `RunWebAction(WebAction::SaveSettings{provider,searxng_url})`
  - `Test(provider)` → `RunWebAction(WebAction::TestSearch{provider, typed_key: (non-empty trimmed input)})`
  - `Close` → `Cancelled`
  - `Stay` → `Pending`
- Both impl `as_any` (+ `as_any_mut` if Task 3 adds it; not needed for transcript-feedback design) returning self.
- Add `show_web_picker(&mut self, snapshot: WebConfigSnapshot)` to `BottomPane` pushing `WebPickerView`.
- Unit-test each view's `handle_key` mapping (construct view, feed KeyEvents, assert ViewOutcome) mirroring model_picker_view tests.

### Task 3: effect plumbing — WebAction outcome variants + AppCallbacks + SystemNotice

**Files:**
- Modify: `tui/src/bottom_pane/view.rs` (add `WebAction` enum + `ViewOutcome::RunWebAction`)
- Modify: `tui/src/bottom_pane/mod.rs` (`BottomPaneOutcome::RunWebAction` + `map_view_outcome` arm)
- Modify: `tui/src/chat_widget.rs` (`ChatOutcome::WebAction(WebAction)` + `on_pane_outcome` arm returns it)
- Modify: `tui/src/app.rs` (`AppCallbacks.on_web_action` + `RataApp::run` match arm + `run_app` param)
- Modify: `tui-core/src/orchestrator_bridge.rs` (`TurnEvent::SystemNotice { body: String, is_error: bool }`)
- Modify: `tui/src/chat_widget.rs` `apply_turn_event` — render `SystemNotice` as a system transcript message (find how "Switching model…" / existing system messages are pushed; reuse `RenderedMessage`'s system/notice variant).

**Design:**
```rust
// view.rs
pub enum WebAction {
    SaveSecret { provider: tool_web::web_search_config::WebSearchProvider, secret: String },
    SaveSettings { provider: tool_web::web_search_config::WebSearchProvider, searxng_url: Option<String> },
    TestSearch { provider: tool_web::web_search_config::WebSearchProvider, typed_key: Option<String> },
}
// ViewOutcome::RunWebAction(WebAction) → BottomPaneOutcome::RunWebAction(WebAction) → ChatOutcome::WebAction(WebAction)
```
- `AppCallbacks` gains `on_web_action: Box<dyn FnMut(WebAction) + 'cb>`; `RataApp::run` match arm calls it; `run_app` gains an `on_web_action` param; test constructors pass a no-op.
- `TurnEvent::SystemNotice` is append-only (add at END of enum). `apply_turn_event` pushes it to the transcript as a system/notice line (is_error → error styling).
- Verify: `cargo test -p tui` + `-p test-harness` compile & pass (the enum additions must be handled in all exhaustive matches).

### Task 4: /web persistence helpers + the async on_web_action closure

**Files:**
- Create: `tui/src/web/persist.rs` (port `web_settings_path`, `save_web_settings_to`, `web_credential_id`)
- Modify: `tui/src/web/mod.rs` (`pub mod persist;`)
- Modify: `apps/cli/src/mode.rs` (`run_ratatui`: build `on_web_action` closure + shared snapshot + pass to `run_app`)

**Design:**
- Port from `git show f4ddad16f:lingxi-code/tui/src/root.rs`: `web_credential_id` (Tavily→`"web:tavily"`, Brave→`"web:brave"`, else None), `web_settings_path` (`dirs::home_dir()` → `memory::lingxi_md::user_config_dir(&h).join("settings.json")`), `save_web_settings_to(path, cfg)` (read-modify-write JSON, pretty + trailing `\n`). Add unit tests for `save_web_settings_to` round-trip (tempdir) and `web_credential_id`.
- In `run_ratatui`, capture `let key_store = tui_build.runtime.provider_key_store.clone();`, `let http = tui_build.runtime.http.clone();`, `let turn_tx = tui_build.turn_tx.clone();`, `let handle = tokio::runtime::Handle::current();`, `let snapshot = shared_snapshot.clone()` (Task 5). Build:
```rust
let on_web_action = move |action: WebAction| {
    let (key_store, http, turn_tx, snapshot, handle2) = (…clones…);
    handle.spawn(async move {
        match action {
            WebAction::SaveSecret { provider, secret } => { /* set_provider_key(web_credential_id) → also save_web_settings_to(provider) → update snapshot flags/active → turn_tx SystemNotice ✓/✗ */ }
            WebAction::SaveSettings { provider, searxng_url } => { /* save_web_settings_to → update snapshot → SystemNotice */ }
            WebAction::TestSearch { provider, typed_key } => { /* port pump_test_web_search body → SystemNotice with resolved.label + count + top hit, update snapshot.last_test */ }
        }
    });
};
```
  Port the `pump_test_web_search` body faithfully (uses `tool_web::web_search_client::{ResolvedWebCredentials, EnvSearchConfig, resolve_client_search_provider_with_credentials, run_client_web_search}`; test query `"current weather Beijing"`, max 3).
- Pass `on_web_action` to `run_app`.

### Task 5: snapshot preload + refresh

**Files:**
- Modify: `apps/cli/src/mode.rs` (`run_ratatui`: build initial snapshot before `run_app`, share it)
- Modify: `tui/src/chat_widget.rs` (`web_snapshot: Option<Arc<Mutex<WebConfigSnapshot>>>` field + `set_web_snapshot` setter; `cmd_web` reads it)
- Modify: `tui/src/app.rs` (`run_app`: `chat_widget.set_web_snapshot(slot)`)

**Design:**
- Before `run_app`, in the async `run_ratatui`: build `WebConfigSnapshot` from `WebSearchConfig::from_settings_json(read ~/.lingxi/settings.json)` for `active`+`searxng_url`, and key presence via `key_store.get_provider_key("web:tavily"/"web:brave").await` → `tavily_key`/`brave_key` bools. Wrap in `Arc<Mutex<_>>`. Pass a clone to both the `on_web_action` closure (Task 4, for post-save refresh) and `chat_widget.set_web_snapshot`.
- `ChatWidget::cmd_web(&mut self, _args)` → lock snapshot, clone, `self.bottom_pane.show_web_picker(snapshot); ChatOutcome::Continue`. If no snapshot slot (shouldn't happen in prod; tests), open from `WebConfigSnapshot::default()`.
- Use `std::sync::Mutex` (sync lock, brief) so `cmd_web` (sync) can lock without await; the async closure locks the same std Mutex.

### Task 6: register /web command + tests

**Files:**
- Modify: `tui/src/command.rs` (add `/web` `SlashCommand` + drop `/web` from the "deliberately not registered" doc note)
- Modify: `tui/src/chat_widget.rs` (`cmd_web` already added Task 5; ensure `run: ChatWidget::cmd_web`)

**Design:**
- Registry entry: `SlashCommand { name: "/web", aliases: &[], description: "Configure web search", args: ArgSpec::None, advertised: true, run: ChatWidget::cmd_web }`. Place near `/mcp`/`/hooks` (tools group).
- The registry single-source test auto-covers completion+help+dispatch. Run `cargo test -p tui command::` + full `-p tui` + `-p test-harness`.

---

## FEATURE 2: /connect (after Feature 1 lands + validated)

Same shape, reusing the effect channel (`ChatOutcome::ConnectAction`) + `TurnEvent::SystemNotice`. Adaptation: OAuth/Copilot do NOT get a live-updating screen — the picker closes and progress (browser-opening, device code + verification URL, ✓/✗) is emitted as `SystemNotice` transcript lines; only API-key entry is a live masked-input view. OAuth path needs USER LIVE BROWSER QA.

- Task 7: port `connect.rs`, `connect_picker.rs`, `connect_method.rs` reducers verbatim into `tui/src/connect/`.
- Task 8: `ConnectPickerView` + `ConnectMethodView` + `ConnectKeyView` (API-key masked input) ratatui views.
- Task 9: thread `provider_auth_methods`/`provider_availability`/`model_providers`/`oauth_connect_driver`/`connect_copilot`/`key_store` into ChatWidget (`cmd_connect` builds picker from the maps) + `run_ratatui`.
- Task 10: `ConnectAction` effect closure — `StoreProviderKey` (set_provider_key(raw provider_id)), `Copilot` (driver.begin → SystemNotice device code + clipboard → poll_to_completion → ✓), `OAuth` (driver.login → ✓/✗), all via `turn_tx` SystemNotice. Dual-method (`anthropic` → [Oauth, ApiKey]) opens ConnectMethodView first.
- Task 11: register `/connect` (bare = picker; `/connect <provider>` = direct route) + optional Ctrl+A. Registry tests.

---

## Progress ledger
- (none yet)
