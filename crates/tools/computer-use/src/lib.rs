//! `tool-computer-use` (M8-P11b) — the `computer` tool.
//!
//! Screen capture + mouse/keyboard/clipboard/app automation, routed to
//! `ctx.computer_control` (`Arc<dyn ComputerControl>`). `None` unless a
//! backend is wired — the real macOS backend
//! (`platform-macos-computer-control`) on desktop, or a mobile `UniFFI` impl.
//! Pure-Rust dispatch.
//!
//! Contract alignment (parity with claude-code's internal `@ant/computer-use-mcp`
//! surface, ground-truthed against the 2.1.218 binary since that package isn't
//! in the leaked source tree): upstream exposes this as ~20 SEPARATE
//! `mcp__computer-use__*` MCP tools plus a "teach mode" tutorial feature
//! (`request_teach_access`/`teach_step`/`teach_batch`) behind a
//! subscription+`GrowthBook`-gated dynamic MCP server. `LingXi` keeps the
//! existing single builtin `computer` tool + `action` enum design (a
//! deliberate, already-reviewed divergence — splitting into N registered
//! tools would ripple through the whole tool-registry/system-prompt stack for
//! no behavioral gain), but the ACTION SET, validation rules, error wording,
//! and the tiered per-app permission model below are ported byte-for-byte
//! where the binary gave concrete ground truth. Teach mode is NOT ported
//! (a distinct, TUI-overlay-shaped feature, out of scope here).
//!
//! Session-scoped permission model (parity with `request_access` /
//! `list_granted_applications` / the app-allowlist + grant-flags the real
//! tool surface enforces): apps are allowed at a `read` / `click` / `full`
//! tier; `clipboardRead` / `clipboardWrite` / `systemKeyCombos` are separate
//! opt-in grants. `request_access` drives the SAME generic Allow/Deny
//! permission-prompt flow every other `LingXi` tool uses (`check_permissions`
//! → `PermissionResult::Ask`) rather than a bespoke approval dialog.

#![forbid(unsafe_code)]
// Dead code kept visible, not swept: this crate had 1 item(s) rustc could
// reach from nothing when the workspace was measured (2026-09-16). The lint
// stays `warn` at the workspace level so a NEW crate still inherits it; this
// allow is scoped here so the count is per crate and repayable by deleting this
// line. This is the category where "named, computed, never wired" hides — some
// of these read like features that were built and never connected. Each wants a
// decision (delete, or wire), not a blanket deletion.
// ⚠️ The count above is ONE macOS, lib-target measurement. It is not a list of
// deletable items — see docs/HANDOFF-dead-code-adjudication-2026-09-17.md,
// which records two near-misses where it said "dead" about live code.
#![allow(dead_code)]

use async_trait::async_trait;
use lingxi_core::host::computer_control::ComputerError;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use std::sync::Mutex;

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::BuiltinToolContext;

mod access_resolver;
mod lock;
mod permission_model;
mod validate;

pub use access_resolver::{
    AutoGrantResolver, ComputerAccessResolver, DenyAllResolver, TuiBridgeResolver,
};
use permission_model::{AppTier, GrantFlags, SessionState};

/// Tool name byte-lock.
pub const TOOL_NAME: &str = "computer";

/// Cross-session lock-held error string (parity with `wrapper.tsx`
/// `formatLockHeld`). `holder` is truncated to the first 8 chars, matching the
/// upstream `holder.slice(0, 8)`. Not currently called anywhere — the actual
/// lock (see [`lock`]) identifies its holder by PID, not a session id, so
/// there is no live call site for this UI-level "who's holding it" phrasing
/// yet; kept for a future approval-time surface (e.g. `request_access`
/// showing who's active) that does have a session id to report.
#[must_use]
pub fn format_lock_held(holder: &str) -> String {
    let short: String = holder.chars().take(8).collect();
    format!(
        "Computer use is in use by another Claude session ({short}…). Wait for that session to finish or run /exit there."
    )
}

/// Tool-call-time lock message (parity with the binary's `cu_lock_held`
/// code path — distinct from [`format_lock_held`], which is the UI-level
/// approval-time message).
pub const LOCK_HELD_AT_CALL: &str = "Another Claude session is currently using the computer. Wait for the user to acknowledge it is finished (stop button in the Claude window), or find a non-computer-use approach if one is readily apparent.";

/// Enter-notification message when the Esc abort hotkey is registered
/// (`wrapper.tsx` `computer_use_enter`).
///
/// Not currently wired to a live call site. It needs a system-wide (NOT
/// terminal-focus-scoped) Esc capture — the whole point is aborting a
/// computer-use action while focus has moved to whatever app the model is
/// driving — which on macOS means a `CGEventTap` in
/// `platform-macos-computer-control` (the only crate here allowed `unsafe`
/// for exactly this kind of native seam). That capability doesn't exist yet
/// and isn't interactively testable in a non-GUI sandbox, so it's
/// deliberately not implemented blind; [`NOTIFY_ENTER_CTRL_C`] is the
/// already-real fallback (the TUI's existing Ctrl+C-cancels-the-turn path
/// works today regardless of which app has focus, as long as the terminal
/// itself is still the foreground window).
pub const NOTIFY_ENTER_ESC: &str = "Claude is using your computer · press Esc to stop";
/// Enter-notification message when only Ctrl+C is available. Same "not yet
/// wired to a live call site" status as [`NOTIFY_ENTER_ESC`] — showing
/// EITHER enter notification needs a terminal-status surface this tool
/// crate has no channel to (`tui-core`'s bridge pattern, e.g.
/// `computer_access_bridge`, is the shape a future one would take).
pub const NOTIFY_ENTER_CTRL_C: &str = "Claude is using your computer · press Ctrl+C to stop";
/// Exit-notification message at turn end (`cleanup.ts` `computer_use_exit`).
/// Same "not yet wired" status — pairs with [`NOTIFY_ENTER_ESC`]/
/// [`NOTIFY_ENTER_CTRL_C`].
pub const NOTIFY_EXIT: &str = "Claude is done using your computer";

/// `ComputerTool` — screenshot + mouse/keyboard/clipboard/app automation.
#[derive(Clone)]
pub struct ComputerTool {
    ctx: BuiltinToolContext,
    state: std::sync::Arc<Mutex<SessionState>>,
    access_resolver: std::sync::Arc<dyn ComputerAccessResolver>,
    /// Resolved once at construction (not re-read per call) — the directory
    /// [`Self::enforce_computer_lock`] locks in. Overridable in tests via
    /// [`Self::with_lock_home`] so they never touch the real
    /// `$HOME/.lingxi` (or a concurrently-running real session's lock).
    lock_home: std::path::PathBuf,
}

impl ComputerTool {
    /// Construct from the builtin tool context, denying every
    /// `request_access` call (no live UI to ask — see
    /// [`Self::with_access_resolver`] for the interactive path).
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self::with_access_resolver(ctx, std::sync::Arc::new(DenyAllResolver))
    }

    /// Construct with an explicit `request_access` resolver — the desktop
    /// composition root wires a [`TuiBridgeResolver`] here when a live TUI is
    /// present, matching how `tool_ui::AskUserQuestionTool` takes its
    /// resolver.
    #[must_use]
    pub fn with_access_resolver(
        ctx: BuiltinToolContext,
        access_resolver: std::sync::Arc<dyn ComputerAccessResolver>,
    ) -> Self {
        Self {
            ctx,
            state: std::sync::Arc::new(Mutex::new(SessionState::default())),
            access_resolver,
            lock_home: lock::lingxi_config_home_dir(),
        }
    }

    /// Redirect the cross-session lock to `home` instead of the real
    /// `$HOME/.lingxi` — test-only, so the test suite never contends with
    /// (or corrupts) an actual concurrently-running session's lock file.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_lock_home(mut self, home: std::path::PathBuf) -> Self {
        self.lock_home = home;
        self
    }
}

/// All actions exposed by the schema. Parity with the upstream tool set
/// (binary-verified: `computer_batch`, `middle_click`, `write_clipboard`,
/// `left_mouse_up`, `list_granted_applications`, `left_click_drag`,
/// `switch_display`, `open_application`, `left_mouse_down`, `hold_key`,
/// `read_clipboard`, `right_click`, `double_click`, `cursor_position`,
/// `left_click`, `triple_click`, `mouse_move`, plus `screenshot`/`zoom`/
/// `type`/`key`/`scroll`/`wait`/`request_access`). `display_size` is a `LingXi`
/// addition (not a standalone upstream tool — upstream folds display size
/// into `screenshot`/`zoom` internally); kept for callers that just want
/// dimensions without a capture.
const ACTIONS: &[&str] = &[
    "screenshot",
    "display_size",
    "cursor_position",
    "mouse_move",
    "left_click",
    "right_click",
    "middle_click",
    "double_click",
    "triple_click",
    "left_click_drag",
    "left_mouse_down",
    "left_mouse_up",
    "type",
    "key",
    "hold_key",
    "scroll",
    "wait",
    "zoom",
    "read_clipboard",
    "write_clipboard",
    "open_application",
    "computer_batch",
    "switch_display",
    "list_granted_applications",
    "request_access",
];

/// Actions valid as items inside a `computer_batch` (everything except the
/// meta/session tools — matches the binary's per-batch-item allow-check).
fn allowed_in_batch(action: &str) -> bool {
    !matches!(
        action,
        "computer_batch" | "request_access" | "list_granted_applications" | "switch_display"
    )
}

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ACTIONS
            },
            // Anthropic computer-use convention: [x, y] pixel tuple.
            "coordinate": {
                "type": "array",
                "items": { "type": "integer", "minimum": 0 },
                "minItems": 2,
                "maxItems": 2
            },
            // Drag origin for `left_click_drag`.
            "start_coordinate": {
                "type": "array",
                "items": { "type": "integer", "minimum": 0 },
                "minItems": 2,
                "maxItems": 2
            },
            // Zoom region: [x0, y0, x1, y1] (top-left, bottom-right corners —
            // matches the binary's validated shape, NOT a width/height box).
            "region": {
                "type": "array",
                "items": { "type": "integer", "minimum": 0 },
                "minItems": 4,
                "maxItems": 4
            },
            // Scroll direction + tick count. `scroll_direction`/`scroll_amount`
            // are the binary-verified field names; `direction`/`amount` are
            // kept as a legacy fallback so existing callers do not break.
            "scroll_direction": {
                "type": "string",
                "enum": ["up", "down", "left", "right"]
            },
            "scroll_amount": { "type": "integer", "minimum": 0, "maximum": 100 },
            "direction": {
                "type": "string",
                "enum": ["up", "down", "left", "right"]
            },
            "amount": { "type": "integer", "minimum": 0 },
            // Seconds to wait / hold a key.
            // Ceiling matches `validate::duration_secs`'s enforced bound —
            // advertised in the schema so the model isn't told a wider range
            // than what actually gets accepted.
            "duration": { "type": "number", "minimum": 0, "maximum": 60 },
            // Text for `type` / key name for `key`/`hold_key` / clipboard write.
            "text": { "type": "string" },
            // Repeat count for `key` (positive integer, max 100).
            "repeat": { "type": "integer", "minimum": 1, "maximum": 100 },
            // Bundle id or display name for `open_application`.
            "bundle_id": { "type": "string" },
            // `computer_batch`: sequential sub-actions, stop on first error.
            "actions": {
                "type": "array",
                "items": { "type": "object" }
            },
            // `switch_display`: a monitor name from the screenshot note, or
            // "auto" to return to automatic selection.
            "display": { "type": "string" },
            // `request_access`: app display names or bundle ids to grant.
            "apps": {
                "type": "array",
                "items": { "type": "string" }
            },
            // `request_access`: one-sentence explanation shown to the user.
            "reason": { "type": "string" },
            // `request_access`: requested tier for `apps` (LingXi addition —
            // upstream computes a per-app "proposedTier" heuristically rather
            // than taking one explicitly; exposing it directly is simpler and
            // still reachable from the tiered model). Defaults to "full".
            "tier": { "type": "string", "enum": ["read", "click", "full"] },
            // `request_access` grant flags — camelCase matches the binary's
            // own (Anthropic-internal-extension) field names verbatim; every
            // other field above follows the public computer-use snake_case
            // convention. This mixed casing is upstream's, not a LingXi typo.
            "clipboardRead": { "type": "boolean" },
            "clipboardWrite": { "type": "boolean" },
            "systemKeyCombos": { "type": "boolean" },
            // Legacy flat fallbacks (kept so existing callers do not break).
            "x": { "type": "integer", "minimum": 0 },
            "y": { "type": "integer", "minimum": 0 },
            "dx": { "type": "integer" },
            "dy": { "type": "integer" }
        },
        "required": ["action"]
    })
});

/// One-line dim summary per action, mirroring `toolRendering.tsx`
/// `RESULT_SUMMARY`. Surfaced to the user, so wording is byte-identical.
fn result_summary(action: &str) -> Option<&'static str> {
    Some(match action {
        "screenshot" | "zoom" => "Captured",
        "request_access" => "Access updated",
        "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click" => {
            "Clicked"
        }
        "type" => "Typed",
        "key" | "left_mouse_down" => "Pressed",
        "hold_key" => "Held",
        "scroll" => "Scrolled",
        "left_click_drag" => "Dragged",
        "left_mouse_up" => "Released",
        "open_application" => "Opened",
        "mouse_move" => "Moved",
        "write_clipboard" => "Written",
        "wait" => "Waited",
        _ => return None,
    })
}

fn map_err(e: &ComputerError) -> ToolError {
    match e {
        ComputerError::PermissionDenied(m) => ToolError::PermissionDenied(m.clone()),
        other => ToolError::Internal(other.to_string()),
    }
}

/// A note guiding the model to `switch_display` when more than one display
/// is connected — the schema's only real way to discover monitor names
/// (there's no standalone `list_displays` action exposed to the model).
/// `None` when there's nothing to switch between (0 or 1 display).
fn multi_display_note(
    displays: &[lingxi_core::host::computer_control::DisplayInfo],
) -> Option<String> {
    if displays.len() < 2 {
        return None;
    }
    let names = displays
        .iter()
        .map(|d| {
            if d.is_primary {
                format!("\"{}\" (primary)", d.name)
            } else {
                format!("\"{}\"", d.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "Multiple displays connected: {names}. Call switch_display with one of these names to target it."
    ))
}

/// Build the `{"type":"image","file":{...}}` result shape for `screenshot`/
/// `zoom` — parity with `tool-file`'s Read-on-image path
/// (`tools/file/src/read.rs`'s `read_image_result`): the real captured
/// pixels ride to the model via `content_blocks`, derived generically from
/// this exact shape by `orchestrator`'s `image_tool_result_blocks`, instead
/// of the metadata-only `{width,height,png_bytes_len}` this used to return —
/// which told the model NOTHING about what was actually on screen, yet
/// didn't stop it from confidently describing a screen it had never
/// actually seen (confirmed via live interactive testing). Downsized/
/// re-encoded through the SAME shared image budget every other tool in this
/// codebase already uses (`tool_api::util::image_budget`), since a raw
/// Retina screenshot (often 3000+ px wide) is well past both Anthropic's
/// image size limit and this crate's own multi-MB context-bloat concern.
fn image_action_result(
    shot: lingxi_core::host::computer_control::Screenshot,
    extra_note: Option<&str>,
) -> Result<Value, ToolError> {
    let original_size = u64::try_from(shot.png_bytes.len()).unwrap_or(u64::MAX);
    let processed =
        tool_api::util::image_budget::process_image(shot.png_bytes).map_err(ToolError::Internal)?;
    let mut file = json!({
        "base64": processed.base64,
        "type": processed.media_type,
        "originalSize": original_size,
    });
    let mut notes: Vec<String> = extra_note.map(str::to_string).into_iter().collect();
    if let Some((ow, oh, dw, dh)) = processed.resized {
        file["dimensions"] = json!({
            "originalWidth": ow,
            "originalHeight": oh,
            "displayWidth": dw,
            "displayHeight": dh,
        });
        // Click/scroll/drag coordinates are always in the REAL screen's
        // pixel space (`width`/`height` below) — not the possibly-downscaled
        // space of the image the model is actually looking at. Without this
        // note the model would have no way to know the two differ, and
        // every subsequent coordinate it picks off the image would land in
        // the wrong place.
        let scale = f64::from(ow) / f64::from(dw.max(1));
        notes.push(format!(
            "Image downscaled from {ow}x{oh} to {dw}x{dh} to fit size limits. Mouse/click coordinates must stay in the ORIGINAL {ow}x{oh} space (this result's width/height) — multiply any coordinate read off the displayed image by {scale:.2}."
        ));
    }
    let mut data = json!({
        "type": "image",
        "file": file,
        "width": shot.width,
        "height": shot.height,
    });
    if !notes.is_empty() {
        data["note"] = json!(notes.join(" "));
    }
    Ok(data)
}

/// Read a `[x, y]` tuple under `key`, falling back to the legacy flat `x`/`y`
/// scalars when reading the `"coordinate"` key. Returns `None` if absent.
fn coord(input: &Value, key: &str) -> Option<(u32, u32)> {
    #[allow(clippy::cast_possible_truncation)] // coordinate space never exceeds u32
    if let Some(arr) = input.get(key).and_then(Value::as_array) {
        if arr.len() == 2 {
            if let (Some(x), Some(y)) = (arr[0].as_u64(), arr[1].as_u64()) {
                return Some((x as u32, y as u32));
            }
        }
        return None;
    }
    // Legacy flat fallback only for the primary `coordinate` field.
    if key == "coordinate" {
        let x = input.get("x").and_then(Value::as_u64);
        let y = input.get("y").and_then(Value::as_u64);
        #[allow(clippy::cast_possible_truncation)] // coordinate space never exceeds u32
        if let (Some(x), Some(y)) = (x, y) {
            return Some((x as u32, y as u32));
        }
    }
    None
}

/// Translate a `scroll_direction`/`scroll_amount` (or legacy `direction`/
/// `amount`) pair into `(dx, dy)` ticks, validating both fields (bounds on
/// `scroll_amount` matter — an unvalidated value used to flow straight
/// through to the native scroll call with no cap). Falls back to the legacy
/// flat `dx`/`dy` when neither `scroll_direction` nor `direction` is
/// supplied. One tick ≈ `amount` (default 3).
fn scroll_delta(input: &Value) -> Result<(i32, i32), ToolError> {
    if let Some(dir) = validate::scroll_direction(input)? {
        let amount = validate::scroll_amount(input)?;
        return Ok(match dir {
            "up" => (0, -amount),
            "down" => (0, amount),
            "left" => (-amount, 0),
            "right" => (amount, 0),
            _ => unreachable!("scroll_direction validated to one of up/down/left/right"),
        });
    }
    #[allow(clippy::cast_possible_truncation)] // scroll delta never exceeds i32
    let dx = input.get("dx").and_then(Value::as_i64).unwrap_or(0) as i32;
    #[allow(clippy::cast_possible_truncation)] // scroll delta never exceeds i32
    let dy = input.get("dy").and_then(Value::as_i64).unwrap_or(0) as i32;
    Ok((dx, dy))
}

#[async_trait]
impl Tool for ComputerTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        // Gate on backend presence: advertising a tool that can only ever
        // fail (every real action returns "not available on this platform")
        // is worse than not listing it. Upstream gates the whole MCP surface
        // behind a subscription tier + GrowthBook flag (`getChicagoEnabled`)
        // — LingXi has neither concept, so backend-presence is the closest
        // faithful analog: "can this actually do anything."
        self.ctx.computer_control.is_some()
    }
    fn max_result_size_chars(&self) -> usize {
        4096
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, input: &Value) -> bool {
        matches!(
            input.get("action").and_then(Value::as_str),
            Some(
                "screenshot"
                    | "display_size"
                    | "cursor_position"
                    | "zoom"
                    | "read_clipboard"
                    | "wait"
                    | "list_granted_applications"
            )
        )
    }

    async fn check_permissions(&self, _input: &Value, _: &ToolUseContext) -> PermissionResult {
        // Parity note: claude-code does NOT permission-prompt per action — the
        // `mcp__computer-use__*` tools are pre-added to allowedTools, and the
        // dedicated `request_access` tool handles session-scoped approval via
        // its own bespoke two-panel dialog (app allowlist + grant-flag
        // checkboxes, or a TCC-permission panel). LingXi's `request_access`
        // owns its OWN interactivity the same way — via `self.access_resolver`
        // inside `handle_request_access`, exactly how `AskUserQuestionTool`
        // pauses on its own resolver rather than routing through this generic
        // gate — so this hook always allows; per-app/per-capability
        // enforcement happens inside `call()` against the session's allowlist
        // + grant flags, and `request_access` itself against the resolver's
        // real answer, not here.
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "computer tool — OS screen-recording/accessibility prompt gates use".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, input: &Value, _: &DescriptionOptions) -> String {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("screenshot");
        format!("Computer action: {action}")
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        "Capture the screen and drive the mouse/keyboard/clipboard/apps.".into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        match input.get("action").and_then(Value::as_str) {
            Some(a) if ACTIONS.contains(&a) => Ok(()),
            Some(a) => Err(ValidationError(format!("unknown action: {a}"))),
            None => Err(ValidationError("`action` is required".into())),
        }
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("screenshot")
            .to_string();

        self.enforce_computer_lock(&action)?;

        // Session-scoped meta actions never touch the ComputerControl seam.
        match action.as_str() {
            "request_access" => return self.handle_request_access(&input).await,
            "list_granted_applications" => return Ok(self.handle_list_granted()),
            "switch_display" => return self.handle_switch_display(&input).await,
            "computer_batch" => return self.handle_batch(&input, &ctx, &progress_tx).await,
            _ => {}
        }

        let data = self.execute_one(&action, &input).await?;
        Ok(finish(data, &action))
    }
}

impl ComputerTool {
    /// Resolve a caller-supplied app identifier (a display name, e.g. what
    /// the model reads off a screenshot, OR an already-correct bundle id) to
    /// the OS-canonical bundle id `enforce_tier` will actually look up against
    /// `frontmost_app()`. Without this, granting "Slack" via `request_access`
    /// and then having Slack become frontmost (reported as bundle id
    /// `com.tinyspeck.slackmacgap`) would silently fail every tier check —
    /// the allowlist key and the lookup key would just never match. Mirrors
    /// the same exact-id-then-case-insensitive-name resolution the macOS
    /// backend's own `open_application` already does internally. Falls back
    /// to the identifier as given when there's no backend to resolve against,
    /// or when nothing in the installed-apps list matches (a still-plausible
    /// bundle id the enumeration didn't happen to cover).
    async fn resolve_app_identifier(&self, name: &str) -> String {
        let Some(cc) = self.ctx.computer_control.as_ref() else {
            return name.to_string();
        };
        let Ok(installed) = cc.list_installed_apps().await else {
            return name.to_string();
        };
        installed
            .iter()
            .find(|a| a.bundle_id == name)
            .or_else(|| {
                installed
                    .iter()
                    .find(|a| a.display_name.eq_ignore_ascii_case(name))
            })
            .map_or_else(|| name.to_string(), |a| a.bundle_id.clone())
    }

    async fn handle_request_access(&self, input: &Value) -> Result<ToolCallResult, ToolError> {
        let raw_apps: Vec<String> = input
            .get("apps")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut resolved_apps = Vec::with_capacity(raw_apps.len());
        for name in &raw_apps {
            resolved_apps.push(self.resolve_app_identifier(name).await);
        }
        let tier = match input.get("tier").and_then(Value::as_str) {
            Some("read") => AppTier::Read,
            Some("click") => AppTier::Click,
            _ => AppTier::Full, // default — matches upstream's "full" unless the model asks for less
        };
        let tcc_state = match self.ctx.computer_control.as_ref() {
            Some(cc) => cc.check_os_permissions().await.and_then(|(acc, rec)| {
                (!acc || !rec).then_some(permission::computer_access::TccState {
                    accessibility: acc,
                    screen_recording: rec,
                })
            }),
            None => None,
        };
        let request = permission::computer_access::ComputerAccessRequest {
            reason: input
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("control your computer")
                .to_string(),
            apps: resolved_apps
                .iter()
                .map(|label| permission::computer_access::RequestedApp {
                    label: label.clone(),
                })
                .collect(),
            tier: tier.to_bridge(),
            clipboard_read: input
                .get("clipboardRead")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            clipboard_write: input
                .get("clipboardWrite")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            system_key_combos: input
                .get("systemKeyCombos")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            tcc_state,
        };
        // Pause here for the user's real decision — the resolver is either a
        // live `TuiBridgeResolver` (real interactive dialog) or the hermetic
        // `DenyAllResolver` (no UI wired: fails closed, grants nothing).
        let response = self.access_resolver.resolve(request).await;

        let mut state = self
            .state
            .lock()
            .map_err(|_| ToolError::Internal("computer-use session state poisoned".into()))?;
        for name in &response.granted_apps {
            state.grant_app(name.clone(), tier);
        }
        state.merge_grant_flags(GrantFlags {
            clipboard_read: response.clipboard_read,
            clipboard_write: response.clipboard_write,
            system_key_combos: response.system_key_combos,
        });
        let granted: Vec<Value> = response.granted_apps.iter().map(|a| json!(a)).collect();
        let denied: Vec<Value> = resolved_apps
            .iter()
            .filter(|a| !response.granted_apps.contains(a))
            .map(|a| json!(a))
            .collect();
        let grant_flags = json!({
            "clipboardRead": state.grant_flags.clipboard_read,
            "clipboardWrite": state.grant_flags.clipboard_write,
            "systemKeyCombos": state.grant_flags.system_key_combos,
        });
        drop(state);
        Ok(finish(
            json!({ "granted": granted, "denied": denied, "grant_flags": grant_flags }),
            "request_access",
        ))
    }

    fn handle_list_granted(&self) -> ToolCallResult {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let apps: Vec<Value> = state
            .allowed_apps
            .iter()
            .map(|a| json!({ "name": a.bundle_id, "tier": a.tier.as_str() }))
            .collect();
        let data = json!({
            "apps": apps,
            "clipboardRead": state.grant_flags.clipboard_read,
            "clipboardWrite": state.grant_flags.clipboard_write,
            "systemKeyCombos": state.grant_flags.system_key_combos,
        });
        finish(data, "list_granted_applications")
    }

    async fn handle_switch_display(&self, input: &Value) -> Result<ToolCallResult, ToolError> {
        let display = input
            .get("display")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("display is required".into()))?;

        if display.eq_ignore_ascii_case("auto") {
            // Best-effort: sync the backend's own pin back to automatic
            // selection too (a prior named switch_display may have pinned
            // it there). `Unsupported` means the backend has no such
            // concept at all (single-display / mobile) — nothing to reset,
            // not a failure. Any other error is real and surfaces.
            if let Some(cc) = self.ctx.computer_control.as_ref() {
                if let Err(e) = cc.select_display(None).await {
                    if !matches!(e, ComputerError::Unsupported(_)) {
                        return Err(map_err(&e));
                    }
                }
            }
            let mut state = self
                .state
                .lock()
                .map_err(|_| ToolError::Internal("computer-use session state poisoned".into()))?;
            state.clear_display_pin();
            drop(state);
            return Ok(finish(
                json!({ "ok": true, "note": "Returned to automatic monitor selection. Call screenshot to continue." }),
                "switch_display",
            ));
        }

        // Resolving a name to a display id needs a live backend; without one
        // there is nothing to switch between (`feature_unavailable` in the
        // binary's own wording for this exact case).
        let Some(cc) = self.ctx.computer_control.as_ref() else {
            return Err(ToolError::Internal(
                "Display switching is not available in this session.".into(),
            ));
        };
        let displays = cc.list_displays().await.map_err(|e| map_err(&e))?;
        let Some(matched) = displays
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(display))
        else {
            let available = if displays.is_empty() {
                "none detected".to_string()
            } else {
                displays
                    .iter()
                    .map(|d| format!("\"{}\"", d.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            return Err(ToolError::InvalidInput(format!(
                "No display named \"{display}\". Available: {available}."
            )));
        };
        let id = matched.id;
        let resolved_name = matched.name.clone();
        cc.select_display(Some(id)).await.map_err(|e| map_err(&e))?;

        let mut state = self
            .state
            .lock()
            .map_err(|_| ToolError::Internal("computer-use session state poisoned".into()))?;
        state.pin_display(id, &resolved_name);
        drop(state);
        Ok(finish(
            json!({ "ok": true, "note": format!("Switched to monitor \"{resolved_name}\". Call screenshot to see it.") }),
            "switch_display",
        ))
    }

    async fn handle_batch(
        &self,
        input: &Value,
        ctx: &ToolUseContext,
        progress_tx: &ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let items = input
            .get("actions")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::InvalidInput("actions must be a non-empty array".into()))?;
        if items.is_empty() {
            return Err(ToolError::InvalidInput(
                "actions must be a non-empty array".into(),
            ));
        }
        let mut results = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let Some(obj) = item.as_object() else {
                return Err(ToolError::InvalidInput(format!(
                    "actions[{i}] must be an object"
                )));
            };
            let Some(sub_action) = obj.get("action").and_then(Value::as_str) else {
                return Err(ToolError::InvalidInput(format!(
                    "actions[{i}].action must be a string"
                )));
            };
            if !allowed_in_batch(sub_action) || !ACTIONS.contains(&sub_action) {
                return Err(ToolError::InvalidInput(format!(
                    "actions[{i}].action=\"{sub_action}\" is not allowed in a batch"
                )));
            }
            let sub_input = Value::Object(obj.clone());
            let _ = (ctx, progress_tx); // batch sub-actions don't emit per-item progress yet
            match self.execute_one(sub_action, &sub_input).await {
                Ok(data) => results.push(json!({ "action": sub_action, "result": data })),
                Err(e) => {
                    return Ok(finish(
                        json!({
                            "stepsCompleted": results.len(),
                            "stepFailed": { "action": sub_action, "error": e.to_string() },
                            "results": results,
                        }),
                        "computer_batch",
                    ));
                }
            }
        }
        Ok(finish(
            json!({ "stepsCompleted": results.len(), "results": results }),
            "computer_batch",
        ))
    }

    /// Enforce the cross-session computer lock (parity with the binary's
    /// `cu_lock_held` gate). Exempts the two purely session-local
    /// bookkeeping actions (`request_access`, `list_granted_applications`) —
    /// neither touches the shared physical machine, so two sessions doing
    /// their OWN permission bookkeeping concurrently is harmless. Every
    /// other action (including `switch_display` and `computer_batch`, which
    /// dispatch outside [`Self::execute_one`]) claims — or re-claims — the
    /// lock for this process, or fails with [`LOCK_HELD_AT_CALL`] when a
    /// different live process already holds it.
    fn enforce_computer_lock(&self, action: &str) -> Result<(), ToolError> {
        if matches!(action, "request_access" | "list_granted_applications") {
            return Ok(());
        }
        #[allow(clippy::cast_possible_wrap)] // real PIDs never approach i32::MAX
        let my_pid = std::process::id() as i32;
        match lock::check(&self.lock_home, my_pid) {
            lock::Holder::Other { .. } => {
                Err(ToolError::PermissionDenied(LOCK_HELD_AT_CALL.to_string()))
            }
            lock::Holder::Free | lock::Holder::Ourselves => {
                lock::claim(&self.lock_home, my_pid);
                Ok(())
            }
        }
    }

    /// Enforce the frontmost-app tier gate for actions that touch the screen.
    /// Read-only/meta actions, and anything called with NO live backend at
    /// all, are waved through — there's nothing to compare against, matching
    /// `check_permissions`'s "OS prompt gates use" stance for a backend-less
    /// session. Once a backend IS present, everything else fails CLOSED: an
    /// empty allowlist, an unresolvable frontmost app, and an ungranted or
    /// under-tiered app are all denied, not silently allowed — a transient
    /// "can't tell what's frontmost" is not the same as "nothing to check".
    async fn enforce_tier(&self, action: &str) -> Result<(), ToolError> {
        let required = match action {
            "right_click" | "middle_click" | "type" | "key" | "hold_key" => Some(AppTier::Full),
            "mouse_move" | "left_click" | "double_click" | "triple_click" | "scroll"
            | "left_click_drag" | "left_mouse_down" | "left_mouse_up" | "cursor_position" => {
                Some(AppTier::Click)
            }
            _ => None,
        };
        let Some(required) = required else {
            return Ok(());
        };
        let Some(cc) = self.ctx.computer_control.as_ref() else {
            return Ok(());
        };
        {
            // Scoped so the guard drops before the `.await` below — held
            // across an await point, a `std::sync::MutexGuard` makes the
            // whole future `!Send`, which `async_trait` requires.
            let state = self
                .state
                .lock()
                .map_err(|_| ToolError::Internal("computer-use session state poisoned".into()))?;
            if state.allowed_apps.is_empty() {
                // Nothing granted yet at all — matches the binary's
                // "No applications are granted for this session. Call
                // request_access first." (allowlist_empty).
                return Err(ToolError::PermissionDenied(
                    "No applications are granted for this session. Call request_access first."
                        .into(),
                ));
            }
        }
        // A live backend that can't currently name the frontmost app
        // (transient OS state, or a process with no bundle id) is "unknown",
        // not "nothing to check" — fail closed rather than silently letting
        // an ungranted app through this window.
        let Ok(Some(front)) = cc.frontmost_app().await else {
            return Err(ToolError::PermissionDenied(
                "Could not determine the frontmost application. Take a fresh screenshot and try again.".into(),
            ));
        };
        let state = self
            .state
            .lock()
            .map_err(|_| ToolError::Internal("computer-use session state poisoned".into()))?;
        match state.tier_for(&front.bundle_id) {
            None => Err(ToolError::PermissionDenied(format!(
                "\"{}\" is not in the allowed applications. Call request_access to add it.",
                front.display_name
            ))),
            Some(tier) if tier < required => Err(ToolError::PermissionDenied(format!(
                "\"{}\" is granted at tier \"{}\"; this action requires tier \"{}\". Call request_access to upgrade it.",
                front.display_name,
                tier.as_str(),
                required.as_str()
            ))),
            Some(_) => Ok(()),
        }
    }

    /// Dispatch one action to the `ComputerControl` seam (or handle it
    /// locally for actions with no host capability, like `wait`). Shared by
    /// the top-level `call()` and `computer_batch`'s sequential loop.
    // One match arm per action, mirroring the flat action-enum schema above —
    // splitting it up would just scatter the dispatch table across more
    // indirection without shrinking it.
    #[allow(clippy::too_many_lines)]
    async fn execute_one(&self, action: &str, input: &Value) -> Result<Value, ToolError> {
        // `wait` carries no host capability — it only pauses. Handled before
        // the seam check so it succeeds even when no ComputerControl is
        // wired, and before the tier gate since it never touches an app.
        if action == "wait" {
            return validate::wait_duration(input)
                .map(|secs| json!({ "ok": true, "waited_seconds": secs }));
        }

        self.enforce_tier(action).await?;

        let cc = self.ctx.computer_control.as_ref().ok_or_else(|| {
            ToolError::Internal("computer-control not available on this platform".into())
        })?;

        match action {
            "screenshot" => {
                let s = cc.screenshot().await.map_err(|e| map_err(&e))?;
                // Best-effort: a backend that can't enumerate displays (or
                // reports just one) simply gets no note — never fail the
                // screenshot itself over this.
                let note = match cc.list_displays().await {
                    Ok(displays) => multi_display_note(&displays),
                    Err(_) => None,
                };
                image_action_result(s, note.as_deref())
            }
            "display_size" => {
                let (w, h) = cc.display_size().await.map_err(|e| map_err(&e))?;
                Ok(json!({ "width": w, "height": h }))
            }
            "mouse_move" => {
                let (x, y) = validate::require_coord(input, "coordinate")?;
                cc.mouse_move(x, y).await.map_err(|e| map_err(&e))?;
                Ok(json!({ "ok": true }))
            }
            "left_click" => {
                let (x, y) = validate::require_coord(input, "coordinate")?;
                cc.left_click(x, y).await.map_err(|e| map_err(&e))?;
                Ok(json!({ "ok": true }))
            }
            "right_click" => {
                let (x, y) = validate::require_coord(input, "coordinate")?;
                cc.right_click(x, y).await.map_err(|e| map_err(&e))?;
                Ok(json!({ "ok": true }))
            }
            "middle_click" => {
                let (x, y) = validate::require_coord(input, "coordinate")?;
                cc.middle_click(x, y).await.map_err(|e| map_err(&e))?;
                Ok(json!({ "ok": true }))
            }
            "double_click" => {
                let (x, y) = validate::require_coord(input, "coordinate")?;
                cc.double_click(x, y).await.map_err(|e| map_err(&e))?;
                Ok(json!({ "ok": true }))
            }
            "triple_click" => {
                let (x, y) = validate::require_coord(input, "coordinate")?;
                cc.triple_click(x, y).await.map_err(|e| map_err(&e))?;
                Ok(json!({ "ok": true }))
            }
            "left_click_drag" => {
                let to = validate::require_coord(input, "coordinate")?;
                let from = coord(input, "start_coordinate");
                cc.drag(from, to).await.map_err(|e| map_err(&e))?;
                Ok(json!({ "ok": true }))
            }
            "left_mouse_down" => {
                {
                    let state = self.state.lock().map_err(|_| {
                        ToolError::Internal("computer-use session state poisoned".into())
                    })?;
                    if state.mouse_button_held {
                        return Err(ToolError::InvalidInput(
                            "mouse button already held, call left_mouse_up first".into(),
                        ));
                    }
                }
                // Only mark "held" once the press actually succeeds — flagging
                // it beforehand would permanently wedge every future
                // left_mouse_down behind a false "already held" error if
                // mouse_down() itself fails (nothing left_mouse_up could ever
                // clear, since nothing is really held).
                cc.mouse_down().await.map_err(|e| map_err(&e))?;
                if let Ok(mut state) = self.state.lock() {
                    state.mouse_button_held = true;
                }
                Ok(json!({ "ok": true }))
            }
            "left_mouse_up" => {
                // Mirror left_mouse_down: only clear "held" once the release
                // actually succeeds. Clearing it unconditionally first would,
                // on a mouse_up() failure, falsely tell the next
                // left_mouse_down the button is free — same bug class as the
                // one already fixed above, just on the release side.
                cc.mouse_up().await.map_err(|e| map_err(&e))?;
                if let Ok(mut state) = self.state.lock() {
                    state.mouse_button_held = false;
                }
                Ok(json!({ "ok": true }))
            }
            "cursor_position" => {
                let (x, y) = cc.cursor_position().await.map_err(|e| map_err(&e))?;
                Ok(json!({ "x": x, "y": y }))
            }
            "type" => {
                let text = validate::require_text(input)?;
                cc.type_text(text).await.map_err(|e| map_err(&e))?;
                Ok(json!({ "ok": true }))
            }
            "key" => {
                let key = validate::require_text(input)?;
                let repeat = validate::key_repeat(input)?;
                self.enforce_system_shortcut_grant(&key)?;
                for _ in 0..repeat {
                    cc.key(key.clone()).await.map_err(|e| map_err(&e))?;
                }
                Ok(json!({ "ok": true, "repeat": repeat }))
            }
            "hold_key" => {
                let key = validate::require_text(input)?;
                let secs = validate::hold_duration(input)?;
                self.enforce_system_shortcut_grant(&key)?;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let ms = (secs * 1000.0) as u64;
                cc.hold_key(key, ms).await.map_err(|e| map_err(&e))?;
                Ok(json!({ "ok": true }))
            }
            "scroll" => {
                let (x, y) = validate::require_coord(input, "coordinate")?;
                let (dx, dy) = scroll_delta(input)?;
                cc.scroll(x, y, dx, dy).await.map_err(|e| map_err(&e))?;
                Ok(json!({ "ok": true }))
            }
            "zoom" => {
                let (x0, y0, x1, y1) = validate::require_region(input)?;
                let s = cc
                    .zoom(x0, y0, x1 - x0, y1 - y0)
                    .await
                    .map_err(|e| map_err(&e))?;
                image_action_result(s, None)
            }
            "read_clipboard" => {
                self.require_grant_flag(GrantFlags::clipboard_read_enabled, "clipboardRead")?;
                let text = cc.read_clipboard().await.map_err(|e| map_err(&e))?;
                Ok(json!({ "text": text }))
            }
            "write_clipboard" => {
                self.require_grant_flag(GrantFlags::clipboard_write_enabled, "clipboardWrite")?;
                let text = validate::require_text(input)?;
                cc.write_clipboard(text).await.map_err(|e| map_err(&e))?;
                Ok(json!({ "ok": true }))
            }
            "open_application" => {
                let name = input
                    .get("bundle_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::InvalidInput("bundle_id is required".into()))?
                    .to_string();
                // Gate against the allowlist BEFORE launching anything — this
                // action had no permission check at all, letting the model
                // open arbitrary apps regardless of what request_access had
                // actually granted (bypassing the whole per-app model this
                // tool otherwise enforces). Any granted tier suffices (the
                // real system doesn't tier-gate opening, only interacting).
                let resolved = self.resolve_app_identifier(&name).await;
                let is_granted = self
                    .state
                    .lock()
                    .map_err(|_| ToolError::Internal("computer-use session state poisoned".into()))?
                    .tier_for(&resolved)
                    .is_some();
                if !is_granted {
                    return Err(ToolError::PermissionDenied(format!(
                        "\"{name}\" is not granted for this session. Call request_access first."
                    )));
                }
                cc.open_application(resolved)
                    .await
                    .map_err(|e| map_err(&e))?;
                Ok(json!({ "ok": true, "opened": name }))
            }
            other => Err(ToolError::InvalidInput(format!("unknown action: {other}"))),
        }
    }

    fn require_grant_flag(
        &self,
        get: impl Fn(GrantFlags) -> bool,
        flag_name: &str,
    ) -> Result<(), ToolError> {
        let state = self
            .state
            .lock()
            .map_err(|_| ToolError::Internal("computer-use session state poisoned".into()))?;
        if get(state.grant_flags) {
            Ok(())
        } else {
            Err(ToolError::PermissionDenied(format!(
                "Clipboard {} is not granted. Request `{flag_name}` via request_access.",
                if flag_name == "clipboardRead" {
                    "read"
                } else {
                    "write"
                }
            )))
        }
    }

    /// System-level shortcuts (quit app, switch app, lock screen, …) need the
    /// `systemKeyCombos` grant regardless of the frontmost app's tier. Best-
    /// effort detection over the most common macOS system chords — the
    /// binary's own detector isn't reconstructable byte-for-byte from strings
    /// alone, so this list is a documented approximation, not a verified
    /// byte-exact port.
    fn enforce_system_shortcut_grant(&self, chord: &str) -> Result<(), ToolError> {
        if !validate::is_system_shortcut(chord) {
            return Ok(());
        }
        let state = self
            .state
            .lock()
            .map_err(|_| ToolError::Internal("computer-use session state poisoned".into()))?;
        if state.grant_flags.system_key_combos {
            Ok(())
        } else {
            Err(ToolError::PermissionDenied(format!(
                "\"{chord}\" is a system-level shortcut. Request the `systemKeyCombos` grant via request_access to use it."
            )))
        }
    }
}

/// Attach the user-facing one-line summary (parity with `RESULT_SUMMARY`) to the
/// result payload and box it into a [`ToolCallResult`].
fn finish(mut data: Value, action: &str) -> ToolCallResult {
    if let Some(summary) = result_summary(action) {
        if let Some(obj) = data.as_object_mut() {
            obj.insert("summary".into(), json!(summary));
        }
    }
    // The `{"type":"image","file":{...}}` shape (`image_action_result`,
    // screenshot/zoom) rides its actual pixels to the model via
    // `content_blocks` (orchestrator's `image_tool_result_blocks`, derived
    // generically from this exact shape) — NOT via the JSON dump
    // `tool_result_to_model_text` would otherwise fall back to (metadata
    // only: width/height/byte-count). `model_content` here is deliberately a
    // short placeholder plus any resize/multi-display note, matching
    // `tool-file`'s Read-on-image path exactly.
    let model_content = (data.get("type").and_then(Value::as_str) == Some("image")).then(|| {
        match data.get("note").and_then(Value::as_str) {
            Some(note) => format!("[Image content provided in tool result.] {note}"),
            None => "[Image content provided in tool result.]".to_string(),
        }
    });
    ToolCallResult {
        data,
        model_content,
        new_messages: vec![],
        context_modifier: None,
        is_error: false,
        mcp_meta: None,
    }
}

/// Register the `computer` tool against `reg`, denying every
/// `request_access` call (no live UI to ask). Composition roots with a live
/// TUI should call [`register_all_with_access_resolver`] instead.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    use std::sync::Arc;
    reg.register_builtin(Arc::new(ComputerTool::new(ctx)));
}

/// Register the `computer` tool with an explicit `request_access` resolver —
/// the desktop composition root's entry point when a live TUI is present
/// (mirrors `tool_ui::register_all_with_ask_resolver`).
pub fn register_all_with_access_resolver(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    access_resolver: std::sync::Arc<dyn ComputerAccessResolver>,
) {
    use std::sync::Arc;
    reg.register_builtin(Arc::new(ComputerTool::with_access_resolver(
        ctx,
        access_resolver,
    )));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_covers_full_action_set() {
        let schema = &*INPUT_SCHEMA;
        let actions = schema["properties"]["action"]["enum"]
            .as_array()
            .expect("action enum");
        for expect in ACTIONS {
            assert!(
                actions.iter().any(|v| v == expect),
                "missing action in schema: {expect}"
            );
        }
    }

    #[test]
    fn schema_has_tuple_and_legacy_coordinate_fields() {
        let props = &INPUT_SCHEMA["properties"];
        assert_eq!(props["coordinate"]["type"], "array");
        assert_eq!(props["start_coordinate"]["type"], "array");
        assert_eq!(props["region"]["minItems"], 4);
        assert_eq!(props["region"]["maxItems"], 4);
        assert_eq!(props["scroll_direction"]["enum"][0], "up");
        assert!(props["scroll_amount"].is_object());
        assert!(props["duration"].is_object());
        assert!(props["bundle_id"].is_object());
        assert!(props["repeat"].is_object());
        assert!(props["actions"].is_object());
        assert!(props["display"].is_object());
        assert!(props["apps"].is_object());
        assert!(props["clipboardRead"].is_object());
        // Legacy flat fallback retained.
        assert_eq!(props["x"]["type"], "integer");
        assert_eq!(props["y"]["type"], "integer");
        assert_eq!(props["dx"]["type"], "integer");
        assert_eq!(props["direction"]["enum"][0], "up");
    }

    #[test]
    fn coord_reads_tuple_form() {
        let input = json!({ "coordinate": [12, 34] });
        assert_eq!(coord(&input, "coordinate"), Some((12, 34)));
        let start = json!({ "start_coordinate": [5, 6] });
        assert_eq!(coord(&start, "start_coordinate"), Some((5, 6)));
    }

    #[test]
    fn coord_falls_back_to_flat_xy_for_coordinate() {
        let input = json!({ "x": 7, "y": 8 });
        assert_eq!(coord(&input, "coordinate"), Some((7, 8)));
        assert_eq!(coord(&input, "start_coordinate"), None);
    }

    #[test]
    fn coord_rejects_malformed_tuple() {
        assert_eq!(coord(&json!({ "coordinate": [1] }), "coordinate"), None);
        assert_eq!(
            coord(&json!({ "coordinate": [1, 2, 3] }), "coordinate"),
            None
        );
        assert_eq!(coord(&json!({}), "coordinate"), None);
    }

    #[test]
    fn scroll_direction_translates_to_delta() {
        assert_eq!(
            scroll_delta(&json!({ "scroll_direction": "up", "scroll_amount": 5 })).unwrap(),
            (0, -5)
        );
        assert_eq!(
            scroll_delta(&json!({ "scroll_direction": "down", "scroll_amount": 2 })).unwrap(),
            (0, 2)
        );
        assert_eq!(
            scroll_delta(&json!({ "direction": "left", "amount": 4 })).unwrap(),
            (-4, 0)
        );
        assert_eq!(
            scroll_delta(&json!({ "scroll_direction": "right" })).unwrap(),
            (3, 0)
        );
    }

    #[test]
    fn scroll_falls_back_to_flat_delta() {
        assert_eq!(
            scroll_delta(&json!({ "dx": 3, "dy": -2 })).unwrap(),
            (3, -2)
        );
        assert_eq!(scroll_delta(&json!({})).unwrap(), (0, 0));
    }

    #[test]
    fn scroll_delta_rejects_unbounded_amount() {
        assert_eq!(
            scroll_delta(&json!({ "scroll_direction": "up", "scroll_amount": 101 }))
                .unwrap_err()
                .to_string(),
            "invalid input: scroll_amount exceeds maximum of 100"
        );
        assert_eq!(
            scroll_delta(&json!({ "scroll_direction": "up", "scroll_amount": -1 }))
                .unwrap_err()
                .to_string(),
            "invalid input: scroll_amount must be a non-negative int"
        );
    }

    #[test]
    fn scroll_delta_rejects_invalid_direction() {
        assert_eq!(
            scroll_delta(&json!({ "scroll_direction": "sideways" }))
                .unwrap_err()
                .to_string(),
            "invalid input: scroll_direction must be 'up', 'down', 'left', or 'right'"
        );
    }

    #[test]
    fn result_summary_matches_upstream_wording() {
        assert_eq!(result_summary("screenshot"), Some("Captured"));
        assert_eq!(result_summary("zoom"), Some("Captured"));
        assert_eq!(result_summary("left_click"), Some("Clicked"));
        assert_eq!(result_summary("middle_click"), Some("Clicked"));
        assert_eq!(result_summary("triple_click"), Some("Clicked"));
        assert_eq!(result_summary("type"), Some("Typed"));
        assert_eq!(result_summary("key"), Some("Pressed"));
        assert_eq!(result_summary("hold_key"), Some("Held"));
        assert_eq!(result_summary("scroll"), Some("Scrolled"));
        assert_eq!(result_summary("left_click_drag"), Some("Dragged"));
        assert_eq!(result_summary("open_application"), Some("Opened"));
        assert_eq!(result_summary("request_access"), Some("Access updated"));
        assert_eq!(result_summary("display_size"), None);
    }

    #[test]
    fn lock_held_string_is_byte_faithful() {
        assert_eq!(
            format_lock_held("abcdefghijklmnop"),
            "Computer use is in use by another Claude session (abcdefgh…). Wait for that session to finish or run /exit there."
        );
        assert_eq!(
            format_lock_held("ab"),
            "Computer use is in use by another Claude session (ab…). Wait for that session to finish or run /exit there."
        );
    }

    #[test]
    fn notification_strings_are_byte_faithful() {
        assert_eq!(
            NOTIFY_ENTER_ESC,
            "Claude is using your computer · press Esc to stop"
        );
        assert_eq!(
            NOTIFY_ENTER_CTRL_C,
            "Claude is using your computer · press Ctrl+C to stop"
        );
        assert_eq!(NOTIFY_EXIT, "Claude is done using your computer");
    }

    #[test]
    fn finish_attaches_summary_only_when_known() {
        let r = finish(json!({ "ok": true }), "left_click");
        assert_eq!(r.data["summary"], "Clicked");
        let r2 = finish(json!({ "ok": true }), "display_size");
        assert!(r2.data.get("summary").is_none());
    }

    #[test]
    fn batch_rejects_disallowed_nested_action() {
        assert!(allowed_in_batch("left_click"));
        assert!(!allowed_in_batch("computer_batch"));
        assert!(!allowed_in_batch("request_access"));
    }
}

/// Integration tests that drive `ComputerTool::call()`/`check_permissions()`
/// end-to-end through a mock `ComputerControl`, rather than testing pure
/// helper functions in isolation. Covers the security/correctness-critical
/// paths this review pass touched: the tier-enforcement gate, mouse-button
/// state bookkeeping around a failing native call, `computer_batch`'s
/// stop-on-first-error semantics, and `request_access`'s name-to-bundle-id
/// resolution.
#[cfg(test)]
mod integration_tests {
    use super::*;
    use lingxi_core::host::computer_control::{
        AppInfo, ComputerControl, ComputerError, DisplayInfo, Screenshot,
    };
    use std::sync::Mutex as StdMutex;

    /// Configurable stand-in for a real backend. Every method not
    /// explicitly exercised by a test returns a cheap default rather than
    /// `Unsupported`, so tests only need to set up the fields their
    /// scenario actually depends on.
    struct MockCc {
        frontmost: StdMutex<Result<Option<AppInfo>, ()>>,
        installed: Vec<AppInfo>,
        mouse_down_ok: bool,
        mouse_up_ok: bool,
        /// The displays `list_displays`/`switch_display` resolve against.
        displays: Vec<DisplayInfo>,
        /// Records the last `select_display` call, so tests can assert the
        /// tool resolved a name to the RIGHT id (not just that resolution
        /// succeeded) without a real backend to observe.
        selected_display: StdMutex<Option<u32>>,
        /// `screenshot()` returns `Unsupported` unless a test opts in — most
        /// scenarios here don't care about the captured image itself.
        screenshot_ok: bool,
    }

    impl Default for MockCc {
        fn default() -> Self {
            Self {
                frontmost: StdMutex::new(Ok(None)),
                installed: vec![],
                mouse_down_ok: true,
                mouse_up_ok: true,
                displays: vec![],
                selected_display: StdMutex::new(None),
                screenshot_ok: false,
            }
        }
    }

    fn app(bundle_id: &str, display_name: &str) -> AppInfo {
        AppInfo {
            bundle_id: bundle_id.to_string(),
            display_name: display_name.to_string(),
        }
    }

    fn display(id: u32, name: &str, is_primary: bool) -> DisplayInfo {
        DisplayInfo {
            id,
            name: name.to_string(),
            width: 1920,
            height: 1080,
            is_primary,
        }
    }

    /// A real, tiny, decodable PNG — `image_action_result` runs every
    /// screenshot through `tool_api::util::image_budget::process_image`,
    /// which needs an actually-decodable image, not an empty placeholder.
    fn tiny_png() -> Vec<u8> {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[async_trait]
    impl ComputerControl for MockCc {
        async fn screenshot(&self) -> Result<Screenshot, ComputerError> {
            if self.screenshot_ok {
                Ok(Screenshot {
                    width: 100,
                    height: 100,
                    png_bytes: tiny_png(),
                })
            } else {
                Err(ComputerError::Unsupported("mock".into()))
            }
        }
        async fn display_size(&self) -> Result<(u32, u32), ComputerError> {
            Err(ComputerError::Unsupported("mock".into()))
        }
        async fn mouse_move(&self, _x: u32, _y: u32) -> Result<(), ComputerError> {
            Ok(())
        }
        async fn left_click(&self, _x: u32, _y: u32) -> Result<(), ComputerError> {
            Ok(())
        }
        async fn right_click(&self, _x: u32, _y: u32) -> Result<(), ComputerError> {
            Ok(())
        }
        async fn double_click(&self, _x: u32, _y: u32) -> Result<(), ComputerError> {
            Ok(())
        }
        async fn type_text(&self, _text: String) -> Result<(), ComputerError> {
            Ok(())
        }
        async fn key(&self, _key: String) -> Result<(), ComputerError> {
            Ok(())
        }
        async fn scroll(&self, _x: u32, _y: u32, _dx: i32, _dy: i32) -> Result<(), ComputerError> {
            Ok(())
        }
        async fn mouse_down(&self) -> Result<(), ComputerError> {
            if self.mouse_down_ok {
                Ok(())
            } else {
                Err(ComputerError::Other("mock mouse_down failure".into()))
            }
        }
        async fn mouse_up(&self) -> Result<(), ComputerError> {
            if self.mouse_up_ok {
                Ok(())
            } else {
                Err(ComputerError::Other("mock mouse_up failure".into()))
            }
        }
        async fn frontmost_app(&self) -> Result<Option<AppInfo>, ComputerError> {
            self.frontmost
                .lock()
                .unwrap()
                .clone()
                .map_err(|()| ComputerError::Other("mock frontmost_app failure".into()))
        }
        async fn list_installed_apps(&self) -> Result<Vec<AppInfo>, ComputerError> {
            Ok(self.installed.clone())
        }
        async fn list_displays(&self) -> Result<Vec<DisplayInfo>, ComputerError> {
            Ok(self.displays.clone())
        }
        async fn select_display(&self, id: Option<u32>) -> Result<(), ComputerError> {
            if let Some(id) = id {
                if !self.displays.iter().any(|d| d.id == id) {
                    return Err(ComputerError::Other(format!("display {id} not found")));
                }
            }
            *self.selected_display.lock().unwrap() = id;
            Ok(())
        }
    }

    /// A fresh, isolated directory for the cross-session lock — every test
    /// gets its own, so the suite never contends with (or corrupts) a real
    /// concurrently-running session's `~/.lingxi/computer-use.lock`.
    fn test_lock_home() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "lingxi-computeruse-test-{}-{n}",
            std::process::id()
        ))
    }

    fn tool_with(mock: MockCc) -> ComputerTool {
        let bus = std::sync::Arc::new(telemetry::AnalyticsBus::new());
        let fs = tool_api::test_support::make_dummy_fs();
        let mut ctx = tool_api::test_support::ctx_for_file_tools(fs, bus, vec![]);
        ctx.computer_control = Some(std::sync::Arc::new(mock));
        // AutoGrantResolver, not the production DenyAllResolver default —
        // these tests exercise the tool's OWN grant/tier bookkeeping (a
        // request_access call followed by tier-gated actions), not the
        // resolver's UI-vs-no-UI behavior (that's covered directly in
        // access_resolver.rs's own tests).
        ComputerTool::with_access_resolver(ctx, std::sync::Arc::new(AutoGrantResolver))
            .with_lock_home(test_lock_home())
    }

    /// Like [`tool_with`], but also hands back the `Arc<MockCc>` so a test
    /// can inspect `selected_display` afterward — the tool itself doesn't
    /// surface the pinned display anywhere in its public JSON responses.
    fn tool_with_shared_cc(mock: MockCc) -> (ComputerTool, std::sync::Arc<MockCc>) {
        let cc = std::sync::Arc::new(mock);
        let bus = std::sync::Arc::new(telemetry::AnalyticsBus::new());
        let fs = tool_api::test_support::make_dummy_fs();
        let mut ctx = tool_api::test_support::ctx_for_file_tools(fs, bus, vec![]);
        ctx.computer_control = Some(cc.clone());
        (
            ComputerTool::with_access_resolver(ctx, std::sync::Arc::new(AutoGrantResolver))
                .with_lock_home(test_lock_home()),
            cc,
        )
    }

    async fn call(tool: &ComputerTool, input: Value) -> Result<ToolCallResult, ToolError> {
        tool.call(
            input,
            tool_api::test_support::fresh_ctx(),
            tool_api::test_support::fresh_tx(),
        )
        .await
    }

    #[tokio::test]
    async fn enforce_tier_denies_when_allowlist_is_empty() {
        let tool = tool_with(MockCc::default());
        let err = call(
            &tool,
            json!({ "action": "left_click", "coordinate": [1, 2] }),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "permission denied: No applications are granted for this session. Call request_access first."
        );
    }

    #[tokio::test]
    async fn enforce_tier_denies_when_frontmost_is_unresolvable() {
        let mut mock = MockCc::default();
        *mock.frontmost.get_mut().unwrap() = Err(());
        let tool = tool_with(mock);
        // Grant something so the allowlist isn't empty — isolates the
        // "can't tell what's frontmost" branch specifically.
        call(
            &tool,
            json!({ "action": "request_access", "apps": ["Anything"] }),
        )
        .await
        .unwrap();
        let err = call(
            &tool,
            json!({ "action": "left_click", "coordinate": [1, 2] }),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "permission denied: Could not determine the frontmost application. Take a fresh screenshot and try again."
        );
    }

    #[tokio::test]
    async fn enforce_tier_denies_an_ungranted_frontmost_app() {
        let mut mock = MockCc::default();
        *mock.frontmost.get_mut().unwrap() = Ok(Some(app("com.other.app", "Other")));
        let tool = tool_with(mock);
        call(
            &tool,
            json!({ "action": "request_access", "apps": ["com.granted.app"] }),
        )
        .await
        .unwrap();
        let err = call(
            &tool,
            json!({ "action": "left_click", "coordinate": [1, 2] }),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "permission denied: \"Other\" is not in the allowed applications. Call request_access to add it."
        );
    }

    #[tokio::test]
    async fn enforce_tier_denies_an_under_tiered_app_for_a_full_tier_action() {
        let mut mock = MockCc::default();
        *mock.frontmost.get_mut().unwrap() = Ok(Some(app("com.granted.app", "Granted")));
        let tool = tool_with(mock);
        call(
            &tool,
            json!({ "action": "request_access", "apps": ["com.granted.app"], "tier": "click" }),
        )
        .await
        .unwrap();
        // `type` requires Full; only Click was granted.
        let err = call(&tool, json!({ "action": "type", "text": "hi" }))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "permission denied: \"Granted\" is granted at tier \"click\"; this action requires tier \"full\". Call request_access to upgrade it."
        );
    }

    #[tokio::test]
    async fn enforce_tier_allows_a_sufficiently_tiered_app() {
        let mut mock = MockCc::default();
        *mock.frontmost.get_mut().unwrap() = Ok(Some(app("com.granted.app", "Granted")));
        let tool = tool_with(mock);
        call(
            &tool,
            json!({ "action": "request_access", "apps": ["com.granted.app"] }),
        )
        .await
        .unwrap();
        let result = call(
            &tool,
            json!({ "action": "left_click", "coordinate": [1, 2] }),
        )
        .await
        .unwrap();
        assert_eq!(result.data["ok"], true);
    }

    #[tokio::test]
    async fn request_access_resolves_display_name_to_bundle_id_for_later_tier_lookups() {
        // Grant by the DISPLAY NAME (all the model can normally read off a
        // screenshot) — the allowlist must still key on the bundle id that
        // `frontmost_app()` reports, or every subsequent tier check fails.
        let mock = MockCc {
            installed: vec![app("com.tinyspeck.slackmacgap", "Slack")],
            ..MockCc::default()
        };
        *mock.frontmost.lock().unwrap() = Ok(Some(app("com.tinyspeck.slackmacgap", "Slack")));
        let tool = tool_with(mock);
        call(
            &tool,
            json!({ "action": "request_access", "apps": ["Slack"] }),
        )
        .await
        .unwrap();
        let result = call(
            &tool,
            json!({ "action": "left_click", "coordinate": [1, 2] }),
        )
        .await
        .unwrap();
        assert_eq!(result.data["ok"], true);
    }

    #[tokio::test]
    async fn open_application_is_denied_without_a_grant() {
        let tool = tool_with(MockCc::default());
        let err = call(
            &tool,
            json!({ "action": "open_application", "bundle_id": "com.foo.bar" }),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "permission denied: \"com.foo.bar\" is not granted for this session. Call request_access first."
        );
    }

    #[tokio::test]
    async fn mouse_button_held_stays_false_when_mouse_down_fails() {
        let mock = MockCc {
            mouse_down_ok: false,
            ..MockCc::default()
        };
        *mock.frontmost.lock().unwrap() = Ok(Some(app("com.granted.app", "Granted")));
        let tool = tool_with(mock);
        call(
            &tool,
            json!({ "action": "request_access", "apps": ["com.granted.app"] }),
        )
        .await
        .unwrap();
        // The failed press must not leave `mouse_button_held` stuck `true` —
        // otherwise every later left_mouse_down would wrongly report
        // "already held" even though nothing is really held.
        assert!(call(&tool, json!({ "action": "left_mouse_down" }))
            .await
            .is_err());
        assert!(
            !tool.state.lock().unwrap().mouse_button_held,
            "a failed mouse_down() must not flag the button as held"
        );
    }

    #[tokio::test]
    async fn mouse_button_held_blocks_a_second_down_until_released() {
        let mut mock = MockCc::default();
        *mock.frontmost.get_mut().unwrap() = Ok(Some(app("com.granted.app", "Granted")));
        let tool = tool_with(mock);
        call(
            &tool,
            json!({ "action": "request_access", "apps": ["com.granted.app"] }),
        )
        .await
        .unwrap();
        call(&tool, json!({ "action": "left_mouse_down" }))
            .await
            .unwrap();
        let err = call(&tool, json!({ "action": "left_mouse_down" }))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid input: mouse button already held, call left_mouse_up first"
        );
        call(&tool, json!({ "action": "left_mouse_up" }))
            .await
            .unwrap();
        // Released — a second down is allowed again.
        assert!(call(&tool, json!({ "action": "left_mouse_down" }))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn mouse_button_held_stays_true_when_mouse_up_fails() {
        let mock = MockCc {
            mouse_up_ok: false,
            ..MockCc::default()
        };
        *mock.frontmost.lock().unwrap() = Ok(Some(app("com.granted.app", "Granted")));
        let tool = tool_with(mock);
        call(
            &tool,
            json!({ "action": "request_access", "apps": ["com.granted.app"] }),
        )
        .await
        .unwrap();
        call(&tool, json!({ "action": "left_mouse_down" }))
            .await
            .unwrap();
        assert!(call(&tool, json!({ "action": "left_mouse_up" }))
            .await
            .is_err());
        assert!(
            tool.state.lock().unwrap().mouse_button_held,
            "a failed mouse_up() must not falsely clear the held flag"
        );
    }

    #[tokio::test]
    async fn computer_batch_stops_on_first_error_and_reports_completed_steps() {
        let mut mock = MockCc::default();
        *mock.frontmost.get_mut().unwrap() = Ok(Some(app("com.granted.app", "Granted")));
        let tool = tool_with(mock);
        // Second action (`type`) needs Full tier; only Click is granted here
        // — it must fail and stop the batch there, not run the third action.
        call(
            &tool,
            json!({ "action": "request_access", "apps": ["com.granted.app"], "tier": "click" }),
        )
        .await
        .unwrap();
        let result = call(
            &tool,
            json!({
                "action": "computer_batch",
                "actions": [
                    { "action": "left_click", "coordinate": [1, 2] },
                    { "action": "type", "text": "hi" },
                    { "action": "left_click", "coordinate": [3, 4] },
                ]
            }),
        )
        .await
        .unwrap();
        assert_eq!(result.data["stepsCompleted"], 1);
        assert!(result.data.get("stepFailed").is_some());
        assert_eq!(
            result.data["results"].as_array().map(Vec::len),
            Some(1),
            "the batch must not run the third action after the second one failed"
        );
    }

    #[tokio::test]
    async fn computer_batch_rejects_empty_actions_array() {
        let tool = tool_with(MockCc::default());
        let err = call(&tool, json!({ "action": "computer_batch", "actions": [] }))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid input: actions must be a non-empty array"
        );
    }

    #[tokio::test]
    async fn check_permissions_always_allows_request_access_owns_its_own_interactivity() {
        // request_access no longer routes through the generic Ask gate — its
        // own resolver (access_resolver.rs) owns the interactive dialog now,
        // the same split AskUserQuestionTool uses. check_permissions is a
        // flat Allow for every action, including request_access itself.
        let tool = tool_with(MockCc::default());
        let ctx = tool_api::test_support::fresh_ctx();
        for action in ["request_access", "left_click"] {
            match tool
                .check_permissions(&json!({ "action": action, "coordinate": [1, 2] }), &ctx)
                .await
            {
                PermissionResult::Allow { .. } => {}
                other => panic!("expected Allow for {action}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn request_access_denies_everything_with_no_resolver_wired() {
        // ComputerTool::new (no explicit resolver) defaults to DenyAllResolver
        // — the safe failure mode when there's no live UI to ask.
        let bus = std::sync::Arc::new(telemetry::AnalyticsBus::new());
        let fs = tool_api::test_support::make_dummy_fs();
        let mut ctx = tool_api::test_support::ctx_for_file_tools(fs, bus, vec![]);
        let mut mock = MockCc::default();
        *mock.frontmost.get_mut().unwrap() = Ok(Some(app("com.granted.app", "Granted")));
        ctx.computer_control = Some(std::sync::Arc::new(mock));
        let tool = ComputerTool::new(ctx).with_lock_home(test_lock_home());
        let result = call(
            &tool,
            json!({ "action": "request_access", "apps": ["Granted"] }),
        )
        .await
        .unwrap();
        assert_eq!(result.data["granted"].as_array().map(Vec::len), Some(0));
        let err = call(
            &tool,
            json!({ "action": "left_click", "coordinate": [1, 2] }),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "permission denied: No applications are granted for this session. Call request_access first."
        );
    }

    #[tokio::test]
    async fn request_access_reports_denied_apps_the_resolver_did_not_grant() {
        // A resolver that grants only PART of what was requested (the real
        // TUI checkbox panel lets the user uncheck individual apps).
        struct PartialGrantResolver;
        #[async_trait]
        impl ComputerAccessResolver for PartialGrantResolver {
            async fn resolve(
                &self,
                request: permission::computer_access::ComputerAccessRequest,
            ) -> permission::computer_access::ComputerAccessResponse {
                permission::computer_access::ComputerAccessResponse {
                    // Grant only the first requested app.
                    granted_apps: request.apps.into_iter().take(1).map(|a| a.label).collect(),
                    clipboard_read: false,
                    clipboard_write: false,
                    system_key_combos: false,
                }
            }
        }
        let bus = std::sync::Arc::new(telemetry::AnalyticsBus::new());
        let fs = tool_api::test_support::make_dummy_fs();
        let mut ctx = tool_api::test_support::ctx_for_file_tools(fs, bus, vec![]);
        ctx.computer_control = Some(std::sync::Arc::new(MockCc::default()));
        let tool =
            ComputerTool::with_access_resolver(ctx, std::sync::Arc::new(PartialGrantResolver))
                .with_lock_home(test_lock_home());
        let result = call(
            &tool,
            json!({ "action": "request_access", "apps": ["com.a.app", "com.b.app"] }),
        )
        .await
        .unwrap();
        assert_eq!(result.data["granted"], json!(["com.a.app"]));
        assert_eq!(result.data["denied"], json!(["com.b.app"]));
    }

    #[tokio::test]
    async fn switch_display_resolves_name_case_insensitively_and_pins_the_backend() {
        let mock = MockCc {
            displays: vec![
                display(1, "Built-in Retina Display", true),
                display(2, "LG UltraFine", false),
            ],
            ..Default::default()
        };
        let (tool, cc) = tool_with_shared_cc(mock);
        let result = call(
            &tool,
            json!({ "action": "switch_display", "display": "lg ultrafine" }),
        )
        .await
        .unwrap();
        assert_eq!(result.data["ok"], true);
        assert!(
            result.data["note"]
                .as_str()
                .unwrap()
                .contains("LG UltraFine"),
            "{:?}",
            result.data
        );
        assert_eq!(*cc.selected_display.lock().unwrap(), Some(2));
    }

    #[tokio::test]
    async fn switch_display_rejects_an_unknown_name_and_lists_the_real_ones() {
        let mock = MockCc {
            displays: vec![display(1, "Built-in Retina Display", true)],
            ..Default::default()
        };
        let tool = tool_with(mock);
        let err = call(
            &tool,
            json!({ "action": "switch_display", "display": "Nonexistent Monitor" }),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Nonexistent Monitor"), "{msg}");
        assert!(msg.contains("Built-in Retina Display"), "{msg}");
    }

    #[tokio::test]
    async fn switch_display_auto_resets_the_backend_pin() {
        let mock = MockCc {
            displays: vec![display(1, "A", true), display(2, "B", false)],
            ..Default::default()
        };
        let (tool, cc) = tool_with_shared_cc(mock);
        call(&tool, json!({ "action": "switch_display", "display": "B" }))
            .await
            .unwrap();
        assert_eq!(*cc.selected_display.lock().unwrap(), Some(2));
        let result = call(
            &tool,
            json!({ "action": "switch_display", "display": "auto" }),
        )
        .await
        .unwrap();
        assert_eq!(result.data["ok"], true);
        assert_eq!(*cc.selected_display.lock().unwrap(), None);
    }

    #[tokio::test]
    async fn switch_display_auto_succeeds_even_when_the_backend_never_supported_pinning() {
        // A minimal backend that doesn't override `select_display` at all —
        // exercises the trait's real default (`Unsupported`), proving `auto`
        // treats that as "nothing to reset" rather than a hard failure.
        struct NoDisplaySupport;
        #[async_trait]
        impl ComputerControl for NoDisplaySupport {
            async fn screenshot(&self) -> Result<Screenshot, ComputerError> {
                Err(ComputerError::Unsupported("mock".into()))
            }
            async fn display_size(&self) -> Result<(u32, u32), ComputerError> {
                Err(ComputerError::Unsupported("mock".into()))
            }
            async fn mouse_move(&self, _x: u32, _y: u32) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn left_click(&self, _x: u32, _y: u32) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn right_click(&self, _x: u32, _y: u32) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn double_click(&self, _x: u32, _y: u32) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn type_text(&self, _text: String) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn key(&self, _key: String) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn scroll(
                &self,
                _x: u32,
                _y: u32,
                _dx: i32,
                _dy: i32,
            ) -> Result<(), ComputerError> {
                Ok(())
            }
        }
        let bus = std::sync::Arc::new(telemetry::AnalyticsBus::new());
        let fs = tool_api::test_support::make_dummy_fs();
        let mut ctx = tool_api::test_support::ctx_for_file_tools(fs, bus, vec![]);
        ctx.computer_control = Some(std::sync::Arc::new(NoDisplaySupport));
        let tool = ComputerTool::with_access_resolver(ctx, std::sync::Arc::new(AutoGrantResolver))
            .with_lock_home(test_lock_home());
        let result = call(
            &tool,
            json!({ "action": "switch_display", "display": "auto" }),
        )
        .await
        .unwrap();
        assert_eq!(result.data["ok"], true);
    }

    #[tokio::test]
    async fn screenshot_note_lists_display_names_when_more_than_one_is_connected() {
        let mock = MockCc {
            displays: vec![
                display(1, "Built-in Retina Display", true),
                display(2, "LG UltraFine", false),
            ],
            screenshot_ok: true,
            ..Default::default()
        };
        let tool = tool_with(mock);
        let result = call(&tool, json!({ "action": "screenshot" }))
            .await
            .unwrap();
        let note = result.data["note"]
            .as_str()
            .expect("note present for multi-display");
        assert!(note.contains("Built-in Retina Display"), "{note}");
        assert!(note.contains("LG UltraFine"), "{note}");
    }

    #[tokio::test]
    async fn screenshot_omits_the_note_for_a_single_display() {
        let mock = MockCc {
            displays: vec![display(1, "Built-in Retina Display", true)],
            screenshot_ok: true,
            ..Default::default()
        };
        let tool = tool_with(mock);
        let result = call(&tool, json!({ "action": "screenshot" }))
            .await
            .unwrap();
        assert!(result.data.get("note").is_none(), "{:?}", result.data);
    }

    /// Spawn a real, cheap, briefly-lived child process to stand in for "a
    /// live pid belonging to another session" — mirrors `lock::tests`'s own
    /// helper (kept separate: that one is private to its module).
    fn spawn_other_process() -> std::process::Child {
        std::process::Command::new(if cfg!(windows) { "cmd" } else { "sleep" })
            .args(if cfg!(windows) {
                vec!["/C", "ping -n 5 127.0.0.1 >NUL"]
            } else {
                vec!["5"]
            })
            .spawn()
            .expect("spawn a short-lived child process")
    }

    #[tokio::test]
    async fn a_live_other_session_sharing_the_lock_home_blocks_real_actions() {
        let home = test_lock_home();
        let mut other = spawn_other_process();
        lock::claim(&home, other.id() as i32);

        let tool = tool_with(MockCc::default()).with_lock_home(home);
        let err = call(&tool, json!({ "action": "screenshot" }))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("permission denied: {LOCK_HELD_AT_CALL}")
        );

        let _ = other.kill();
        let _ = other.wait();
    }

    #[tokio::test]
    async fn request_access_and_list_granted_bypass_the_lock_entirely() {
        let home = test_lock_home();
        let mut other = spawn_other_process();
        lock::claim(&home, other.id() as i32);

        let tool = tool_with(MockCc::default()).with_lock_home(home);
        // Neither of these touches the shared physical machine, so they must
        // succeed even while a different live session holds the lock.
        call(
            &tool,
            json!({ "action": "request_access", "apps": ["Slack"] }),
        )
        .await
        .expect("request_access is exempt from the cross-session lock");
        call(&tool, json!({ "action": "list_granted_applications" }))
            .await
            .expect("list_granted_applications is exempt from the cross-session lock");

        let _ = other.kill();
        let _ = other.wait();
    }

    #[tokio::test]
    async fn a_stale_lock_from_a_dead_session_is_taken_over_rather_than_blocking() {
        let home = test_lock_home();
        let mut other = spawn_other_process();
        let other_pid = other.id() as i32;
        lock::claim(&home, other_pid);
        let _ = other.kill();
        let _ = other.wait(); // now genuinely dead — a stale lock, not a live holder

        let tool = tool_with(MockCc {
            screenshot_ok: true,
            ..Default::default()
        })
        .with_lock_home(home.clone());
        call(&tool, json!({ "action": "screenshot" }))
            .await
            .expect("a dead holder's lock must be treated as stale, not blocking");
        // We took it over — it's now recorded as OUR pid, not the dead one.
        #[allow(clippy::cast_possible_wrap)]
        let my_pid = std::process::id() as i32;
        assert_eq!(lock::check(&home, my_pid), lock::Holder::Ourselves);
    }
}
