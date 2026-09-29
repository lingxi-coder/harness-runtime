//! Deferred-tool loading state — the shared spine of the Tool Search pipeline.
//!
//! claude-code 2.1.216 runs a "tool search" mode in which undiscovered tools
//! whose `shouldDefer === true` are omitted from the request tool list and the
//! model pulls matching definitions into context via the `ToolSearch` tool.
//! Once discovered, a definition returns to the wire with
//! `defer_loading: true` so the provider can retain dynamic-tool semantics.
//! Two facts drive this port:
//!
//! - the DEFERRAL PREDICATE (binary): `if (e.name === "EnterWorktree" &&
//!   process.env.CLAUDE_CODE_SESSION_KIND === "bg") return false; return
//!   e.shouldDefer === true` — every tool that reports `should_defer()` is
//!   deferred, EXCEPT `EnterWorktree` inside a `bg` session.
//! - the LIFECYCLE: once the model discovers a deferred tool (via
//!   `ToolSearch`), its full schema stays present on subsequent turns and the
//!   discovered name survives resume/compaction.
//!
//! [`DeferralState`] is the single object shared between the wire serializer
//! ([`crate::wire::apply_defer_loading`]) and the `ToolSearch` consumer so both
//! ends stay consistent: the wire omits exactly the undiscovered set
//! `ToolSearch` searches, and marking a tool loaded makes it visible on the
//! next turn's wire. It is OWNED by the
//! [`ToolRegistry`](crate::registry::ToolRegistry),
//! which already flows to the orchestrator's wire assembly and to the tool.
//!
//! [`mode_from_env`] reads `LINGXI_ENABLE_TOOL_SEARCH` /
//! `ENABLE_TOOL_SEARCH` (claude-code's env name) and the
//! `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS` kill switch. Like claude-code, an
//! unset enable variable selects `tst`; request assembly subsequently disables
//! it for unsupported models/providers or when `ToolSearch` was denied.

use crate::tool_trait::Tool;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

/// Byte-locked name of the worktree-entry tool (claude-code `k9e`), the sole
/// `shouldDefer` tool that is NOT deferred inside a `bg` session.
pub const ENTER_WORKTREE_TOOL_NAME: &str = "EnterWorktree";

/// Tool-search mode (claude-code `e$r()` result: `standard` / `tst` /
/// `tst-auto`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSearchMode {
    /// Tool search off — nothing is deferred (claude-code `"standard"`).
    Standard,
    /// Tool search on (claude-code `"tst"` / `"tst-auto"`).
    Enabled,
    /// Enable only when deferred schemas are estimated to save at least this
    /// percentage of the active model context window.
    Auto {
        /// Minimum estimated context-window percentage saved by deferral.
        percentage: u8,
    },
}

impl ToolSearchMode {
    /// Whether deferral is active in this mode.
    #[must_use]
    pub fn is_enabled(self) -> bool {
        !matches!(self, ToolSearchMode::Standard)
    }
}

/// Parse the tool-search mode from an enable value and the experimental-betas
/// kill switch. Mirrors the shape of claude-code `e$r()`.
///
/// Byte-faithful port of claude-code `qzr()` (with `Yos`/`B5g`):
/// - kill switch (`Q_e()`) set → `Standard`.
/// - `auto:N` (case-sensitive) → `Yos` parses N and clamps to `0..=100`:
///   `N==0` → `Enabled`, `N==100` (incl. any `N>100`) → `Standard`, otherwise
///   `Auto { percentage: N }`.
/// - `auto` (case-sensitive) → `Auto { percentage: 10 }`.
/// - a defined-falsy value (`0`/`false`/`no`/`off`) → `Standard`.
/// - EVERYTHING else — unset, empty, truthy, `tst`, `tst-auto`, or any
///   unrecognized string — → `Enabled` (the oracle's `Jt`-then-`return "tst"`
///   arms both resolve to enabled; only a defined-falsy value diverges).
///
/// Notably `tst-auto` as an INPUT is enabled (not auto — `B5g("tst-auto")` is
/// false and it is truthy), `auto:99999` is `Standard` (clamped to 100, not a
/// wrapped-integer `Auto`), and an unrecognized value is enabled (not standard).
#[must_use]
pub fn mode_from_values(enable: Option<&str>, disable_experimental_betas: bool) -> ToolSearchMode {
    // Q_e() kill switch.
    if disable_experimental_betas {
        return ToolSearchMode::Standard;
    }
    // Yos(e): `auto:N` clamped to 0..=100; anything else → None. Case-SENSITIVE.
    let auto_pct = enable.and_then(parse_auto_percentage);
    match auto_pct {
        Some(0) => return ToolSearchMode::Enabled, // t === 0 → "tst"
        Some(100) => return ToolSearchMode::Standard, // t === 100 → "standard"
        _ => {}
    }
    // B5g(e): e && (e === "auto" || e.startsWith("auto:")) → "tst-auto".
    if let Some(e) = enable {
        if e == "auto" || e.starts_with("auto:") {
            return ToolSearchMode::Auto {
                percentage: auto_pct.unwrap_or(10),
            };
        }
    }
    // `if(Jt(e))return"tst"` and the final `return"tst"` are the same outcome;
    // only a defined-falsy value (`nu`) diverges to "standard".
    if lingxi_core::host::env::is_env_defined_falsy(enable) {
        return ToolSearchMode::Standard;
    }
    ToolSearchMode::Enabled
}

/// `Yos(e)`: parse a case-sensitive `auto:N` value, clamped to `0..=100`.
/// Returns `None` for any value that does not start with `auto:` or whose `N`
/// fails to parse (`parseInt` NaN), so the caller's `B5g` still classifies a
/// malformed `auto:xxx` as auto with the default percentage.
fn parse_auto_percentage(e: &str) -> Option<u8> {
    let rest = e.strip_prefix("auto:")?;
    // `Yos(e)`: `Ld(e.slice(5))` = the full `WLm(t) ?? parseInt(t,10)` coercion
    // (scientific-notation + digit-separator forms, NOT just a leading
    // `parseInt`), via the already-ported `lingxi_core::host::env::parse_int_env`.
    let r = lingxi_core::host::env::parse_int_env(rest);
    if r.is_nan() {
        // Yos: `if(isNaN(r)) return null`.
        return None;
    }
    // `Math.max(0, Math.min(100, r))`.
    Some(r.clamp(0.0, 100.0) as u8)
}

/// Read [`mode_from_values`] from the live environment. Prefers the rebranded
/// `LINGXI_ENABLE_TOOL_SEARCH`, falling back to claude-code's bare
/// `ENABLE_TOOL_SEARCH`; kill switch is `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS`
/// (kept, matching `llm-runtime`'s beta gate).
#[must_use]
pub fn mode_from_env() -> ToolSearchMode {
    let enable = std::env::var("LINGXI_ENABLE_TOOL_SEARCH")
        .ok()
        .or_else(|| std::env::var("ENABLE_TOOL_SEARCH").ok());
    let disable_betas = lingxi_core::host::env::is_env_truthy(
        std::env::var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS")
            .ok()
            .as_deref(),
    );
    mode_from_values(enable.as_deref(), disable_betas)
}

/// Shared deferral state for a session: the tool-search mode, the `bg`-session
/// flag (for the `EnterWorktree` exception), and the set of tool names the model
/// has discovered via `ToolSearch` this session.
pub struct DeferralState {
    mode: ToolSearchMode,
    session_kind_bg: bool,
    loaded: RwLock<HashSet<String>>,
    auto_active: AtomicBool,
    request_supported: AtomicBool,
    /// Deferred-tool names already ANNOUNCED to the model as available this
    /// session (claude-code `A1s` accumulator `s`, replayed from prior
    /// `deferred_tools_delta` attachments). We track it directly instead of
    /// scanning history because the port does not persist the transient
    /// reminder into `session.history`.
    announced: RwLock<HashSet<String>>,
    /// Deferred-tool names ever announced as GENUINELY NEW (not via a reconnect)
    /// this session (claude-code `A1s` accumulator `a`). Distinguishes a
    /// first-time announcement (`addedLines`) from a re-appearance (`readded`).
    ever_added: RwLock<HashSet<String>>,
}

impl DeferralState {
    /// Construct with an explicit mode and `bg`-session flag.
    #[must_use]
    pub fn new(mode: ToolSearchMode, session_kind_bg: bool) -> Self {
        Self {
            mode,
            session_kind_bg,
            loaded: RwLock::new(HashSet::new()),
            auto_active: AtomicBool::new(false),
            request_supported: AtomicBool::new(true),
            announced: RwLock::new(HashSet::new()),
            ever_added: RwLock::new(HashSet::new()),
        }
    }

    /// A disabled state (nothing deferred) — the default for tests and any
    /// non-tool-search session.
    #[must_use]
    pub fn disabled() -> Self {
        Self::new(ToolSearchMode::Standard, false)
    }

    /// Construct from the live environment: [`mode_from_env`] plus the
    /// `LINGXI_SESSION_KIND == "bg"` flag (claude-code `CLAUDE_CODE_SESSION_KIND`).
    #[must_use]
    pub fn from_env() -> Self {
        let bg = std::env::var("LINGXI_SESSION_KIND").ok().as_deref() == Some("bg");
        Self::new(mode_from_env(), bg)
    }

    /// The tool-search mode.
    #[must_use]
    pub fn mode(&self) -> ToolSearchMode {
        self.mode
    }

    /// Whether deferral is active.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        if !self.request_supported.load(Ordering::Acquire) {
            return false;
        }
        match self.mode {
            ToolSearchMode::Enabled => true,
            ToolSearchMode::Auto { .. } => self.auto_active.load(Ordering::Acquire),
            ToolSearchMode::Standard => false,
        }
    }

    /// Publish whether the active request can carry `defer_loading` and
    /// `tool_reference` blocks. This is recomputed on every model/profile/tool
    /// change, so switching away from Anthropic (or denying `ToolSearch`)
    /// immediately restores the complete inline tool list.
    pub fn set_request_supported(&self, supported: bool) {
        self.request_supported.store(supported, Ordering::Release);
    }

    /// Configured automatic threshold, if this session is in auto mode.
    #[must_use]
    pub fn auto_percentage(&self) -> Option<u8> {
        match self.mode {
            ToolSearchMode::Auto { percentage } => Some(percentage),
            ToolSearchMode::Standard | ToolSearchMode::Enabled => None,
        }
    }

    /// Publish the current turn's auto-threshold result before building the
    /// searchable view and applying wire markers.
    pub fn set_auto_active(&self, active: bool) {
        if matches!(self.mode, ToolSearchMode::Auto { .. }) {
            self.auto_active.store(active, Ordering::Release);
        }
    }

    /// Mark tool names as discovered (pulled into context via `ToolSearch`), so
    /// their definitions become visible on subsequent turns.
    pub fn mark_loaded<I, S>(&self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut g = self
            .loaded
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for n in names {
            g.insert(n.into());
        }
    }

    /// Replace the session's discovered-tool snapshot atomically. Resume and
    /// `/clear` must not merge a previous conversation's ToolSearch state into
    /// the newly adopted session.
    pub fn replace_loaded<I, S>(&self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut g = self
            .loaded
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.clear();
        g.extend(names.into_iter().map(Into::into));
        // The newly adopted session has not announced anything yet: the
        // deferred-tools delta must re-announce its searchable set on the first
        // post-resume/-clear turn rather than treating the prior conversation's
        // announcements as current.
        drop(g);
        self.announced
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.ever_added
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Whether a tool has been discovered this session.
    #[must_use]
    pub fn is_loaded(&self, name: &str) -> bool {
        self.loaded
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(name)
    }

    /// Snapshot deferred-tool names already discovered in this session, sorted for
    /// compact-boundary persistence (`preCompactDiscoveredTools`).
    #[must_use]
    pub fn loaded_tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .loaded
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// The claude-code deferral predicate applied to a concrete tool for THIS
    /// turn's wire.
    #[must_use]
    pub fn should_defer_tool(&self, tool: &dyn Tool) -> bool {
        self.should_defer(tool.name(), tool.should_defer())
    }

    /// The claude-code deferral predicate by name + `should_defer` flag:
    /// `enabled && candidate && !loaded`, with the `EnterWorktree`/`bg`
    /// exception applied first. This identifies schemas not yet discovered.
    #[must_use]
    pub fn should_defer(&self, name: &str, wants_defer: bool) -> bool {
        if !self.is_enabled() {
            return false;
        }
        self.wants_defer(name, wants_defer) && !self.is_loaded(name)
    }

    /// Candidate predicate independent of the current auto-threshold decision.
    #[must_use]
    pub fn wants_defer(&self, name: &str, wants_defer: bool) -> bool {
        if name == ENTER_WORKTREE_TOOL_NAME && self.session_kind_bg {
            return false;
        }
        wants_defer
    }

    /// Compute the deferred-tools delta for THIS outgoing model step against the
    /// previously announced set, then fold the result into the announced /
    /// ever-added tracking.
    ///
    /// Byte-faithful port of the claude-code `A1s` accumulation (the
    /// deferred-tools subset; the `pendingMcpServers` / `needsAuthMcpServers` /
    /// `failedMcpServers` branches are not modeled here). `current` is the
    /// currently-deferred (undiscovered) tool-name set — the oracle
    /// `g = messages.filter(LK)`, i.e. the searchable view minus tools already
    /// loaded this session.
    ///
    /// MUTATES the announced / ever-added tracking, so it must be invoked at most
    /// ONCE per outgoing model step (retries of the same step reuse the cached
    /// result). The next step compares against the now-advanced state, which is
    /// how a re-request with an unchanged deferred set yields an empty delta.
    #[must_use]
    pub fn compute_deferred_delta(&self, current: &[String]) -> DeferredToolsDelta {
        let current_set: HashSet<&str> = current.iter().map(String::as_str).collect();
        let mut announced = self
            .announced
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ever_added = self
            .ever_added
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let loaded = self
            .loaded
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // y = current \ announced — names newly available this step.
        let mut new_names: Vec<String> = current
            .iter()
            .filter(|n| !announced.contains(n.as_str()))
            .cloned()
            .collect();
        // E (addedLines) = current \ ever_added — never announced-as-new.
        let mut added_lines: Vec<String> = current
            .iter()
            .filter(|n| !ever_added.contains(n.as_str()))
            .cloned()
            .collect();
        // v (readdedNames) = y ∩ ever_added — announced earlier, gone, now back.
        let mut readded: Vec<String> = new_names
            .iter()
            .filter(|n| ever_added.contains(n.as_str()))
            .cloned()
            .collect();
        // S (removedNames) = announced \ current \ loaded — was announced, no
        // longer deferred, and not merely discovered (loaded stays present in
        // the full tool set `_`, so the oracle does not report it removed).
        let mut removed: Vec<String> = announced
            .iter()
            .filter(|n| !current_set.contains(n.as_str()) && !loaded.contains(n.as_str()))
            .cloned()
            .collect();

        new_names.sort();
        new_names.dedup();
        added_lines.sort();
        added_lines.dedup();
        readded.sort();
        readded.dedup();
        removed.sort();
        removed.dedup();

        let added_any = !new_names.is_empty();

        // Fold state exactly as the oracle replay does once this delta is stored:
        //   addedNames AN = dedup(y ∪ E);
        //   announced := (announced ∪ AN) \ S;
        //   ever_added := ever_added ∪ (AN \ v).
        // AN ∩ S = ∅ (AN ⊆ current, S disjoint from current), so order is safe.
        for n in new_names.iter().chain(added_lines.iter()) {
            announced.insert(n.clone());
        }
        for n in &removed {
            announced.remove(n);
        }
        let readded_set: HashSet<&str> = readded.iter().map(String::as_str).collect();
        for n in new_names.iter().chain(added_lines.iter()) {
            if !readded_set.contains(n.as_str()) {
                ever_added.insert(n.clone());
            }
        }

        DeferredToolsDelta {
            added_lines,
            readded,
            removed,
            added_any,
        }
    }
}

/// Byte-locked tool name the deferred-tools reminder text refers to (oracle
/// `ZE = "ToolSearch"`).
const TOOL_SEARCH_TOOL_NAME: &str = "ToolSearch";

/// Oracle `oP`: the removed-tool count above which the notice is summarized
/// (grouped `mcp__<server>__*` counts) rather than listed one per line.
const DEFERRED_SUMMARY_THRESHOLD: usize = 30;

/// Oracle `Mdn`: the ambient-context trailer appended after a removed-tools
/// notice. `\u{2014}` is the U+2014 em dash present in the binary literal.
const AMBIENT_CONTEXT_TRAILER: &str = "This is ambient context \u{2014} do not narrate it to the user unless they ask or it is directly relevant to their request.";

/// One deferred-tools delta (oracle `deferred_tools_delta` attachment, restricted
/// to the tool-set fields). Produced by [`DeferralState::compute_deferred_delta`]
/// and rendered by [`DeferredToolsDelta::render_reminder`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DeferredToolsDelta {
    /// Genuinely-new (never announced-as-new) deferred tool names, sorted
    /// (oracle `addedLines` = `E.map(jas).sort()`). Listed in the "now
    /// available via ToolSearch" notice.
    pub added_lines: Vec<String>,
    /// Deferred tool names announced earlier, gone, and now back, sorted (oracle
    /// `readdedNames` = `v.sort()`). Listed in the "available again" notice.
    pub readded: Vec<String>,
    /// Previously-announced names no longer available and not loaded, sorted
    /// (oracle `removedNames` = `S.sort()`). Listed in the "no longer available"
    /// notice.
    pub removed: Vec<String>,
    /// Whether any newly-available name appeared (oracle `y`); part of the emit
    /// gate (`v ⊆ y`, so a pure re-add still sets this).
    added_any: bool,
}

impl DeferredToolsDelta {
    /// Whether this delta yields no reminder (oracle: `y`, `E`, and `S` all
    /// empty return `null`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.added_any && self.added_lines.is_empty() && self.removed.is_empty()
    }

    /// Render the `<system-reminder>` body for this delta, byte-faithful to the
    /// claude-code `deferred_tools_delta` attachment renderer (`BC(i.join("\n\n"))`).
    /// `None` when nothing changed.
    #[must_use]
    pub fn render_reminder(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let tool = TOOL_SEARCH_TOOL_NAME;
        let mut parts: Vec<String> = Vec::new();
        // "now available via ToolSearch" — oracle guard `r.length>0 && n.length>0`
        // with `r = addedLines`; `addedNames ⊇ addedLines`, so `addedLines`
        // non-empty is the effective condition, and the list uses `addedLines`.
        if !self.added_lines.is_empty() {
            parts.push(format!(
                "The following deferred tools are now available via {tool}. Their schemas are NOT loaded \u{2014} calling them directly will fail with InputValidationError. Use {tool} with query \"select:<name>[,<name>...]\" to load tool schemas before calling them:\n{names}",
                tool = tool,
                names = self.added_lines.join("\n"),
            ));
        }
        // "available again (MCP server reconnected …)" — oracle readddedNames branch.
        if !self.readded.is_empty() {
            let verb = if self.readded.len() == 1 {
                " is"
            } else {
                "s are"
            };
            parts.push(format!(
                "{count} deferred tool{verb} available again (MCP server reconnected \u{2014} names announced earlier in this conversation): {names}. Load via {tool} as before.",
                count = self.readded.len(),
                verb = verb,
                names = group_mcp_names(&self.readded),
                tool = tool,
            ));
        }
        // "no longer available (… disconnected)" — oracle removedNames branch,
        // followed by the ambient-context trailer (`Mdn`).
        if !self.removed.is_empty() {
            if self.removed.len() > DEFERRED_SUMMARY_THRESHOLD {
                parts.push(format!(
                    "{count} deferred tools are no longer available (MCP server disconnected): {names}. Do not search for them \u{2014} {tool} will return no match.",
                    count = self.removed.len(),
                    names = group_mcp_names(&self.removed),
                    tool = tool,
                ));
            } else {
                parts.push(format!(
                    "The following deferred tools are no longer available (their MCP server disconnected). Do not search for them \u{2014} {tool} will return no match:\n{names}",
                    tool = tool,
                    names = self.removed.join("\n"),
                ));
            }
            parts.push(AMBIENT_CONTEXT_TRAILER.to_string());
        }
        if parts.is_empty() {
            return None;
        }
        Some(format!(
            "<system-reminder>\n{}\n</system-reminder>",
            parts.join("\n\n")
        ))
    }
}

/// Group tool names for the "available again" / "no longer available" notices,
/// byte-faithful to claude-code `AIo`: collapse `mcp__<server>__<tool>` names to
/// a single `mcp__<server>__*` bucket, count duplicates, sort by bucket key, and
/// render `key (n)` when a bucket has more than one member (else just `key`),
/// joined with `", "`.
fn group_mcp_names(names: &[String]) -> String {
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for name in names {
        // `r.split("__",2).join("__")` keeps the first two `__`-delimited
        // segments (`mcp__<server>`), then appends `__*`.
        let key = if name.starts_with("mcp__") {
            let mut it = name.splitn(3, "__");
            let a = it.next().unwrap_or("");
            let b = it.next().unwrap_or("");
            format!("{a}__{b}__*")
        } else {
            name.clone()
        };
        *counts.entry(key).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .map(|(k, n)| if n > 1 { format!("{k} ({n})") } else { k })
        .collect::<Vec<_>>()
        .join(", ")
}

impl Default for DeferralState {
    fn default() -> Self {
        Self::disabled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_defaults_on_when_unset() {
        assert_eq!(mode_from_values(None, false), ToolSearchMode::Enabled);
    }

    #[test]
    fn mode_truthy_enables() {
        for v in ["1", "true", "on", "yes", "TST", "tst", "auto:0"] {
            assert_eq!(
                mode_from_values(Some(v), false),
                ToolSearchMode::Enabled,
                "{v:?} should enable"
            );
        }
    }

    #[test]
    fn auto_mode_parses_threshold_edges_and_starts_inactive() {
        assert_eq!(
            mode_from_values(Some("auto"), false),
            ToolSearchMode::Auto { percentage: 10 }
        );
        assert_eq!(
            mode_from_values(Some("auto:35junk"), false),
            ToolSearchMode::Auto { percentage: 35 }
        );
        // A malformed `auto:xxx` is still auto (B5g), with the default percentage.
        assert_eq!(
            mode_from_values(Some("auto:xyz"), false),
            ToolSearchMode::Auto { percentage: 10 }
        );
        assert_eq!(
            mode_from_values(Some("auto:100"), false),
            ToolSearchMode::Standard
        );
        // N > 100 clamps to 100 → Standard (NOT a wrapped-integer Auto).
        assert_eq!(
            mode_from_values(Some("auto:99999"), false),
            ToolSearchMode::Standard
        );
        // `Ld` uses the full WLm ?? parseInt coercion, so scientific-notation
        // and digit-separator forms parse (a leading-digits-only parser would
        // read `auto:1e2` as 1 → Auto{1}). `1e2`/`1_0_0` = 100 → Standard.
        assert_eq!(
            mode_from_values(Some("auto:1e2"), false),
            ToolSearchMode::Standard
        );
        // A valid 3-digit-group separator form (1_000 = 1000, clamped to 100).
        assert_eq!(
            mode_from_values(Some("auto:1_000"), false),
            ToolSearchMode::Standard
        );
        let state = DeferralState::new(ToolSearchMode::Auto { percentage: 10 }, false);
        assert!(!state.is_enabled());
        state.set_auto_active(true);
        assert!(state.is_enabled());
    }

    #[test]
    fn qzr_fallthrough_enables_non_falsy_values() {
        // The oracle's `Jt`-then-`return "tst"` arms both enable, so any value
        // that is neither auto-form nor DEFINED-falsy is enabled — including the
        // literal `tst-auto` (not auto), an empty string, and unrecognized values.
        for v in ["tst-auto", "", "enabled", "xyz", "TST"] {
            assert_eq!(
                mode_from_values(Some(v), false),
                ToolSearchMode::Enabled,
                "{v:?} should enable via the qzr fallthrough"
            );
        }
    }

    #[test]
    fn mode_falsy_disables() {
        // Only DEFINED-falsy values disable; an empty string is NOT defined-falsy.
        for v in ["0", "false", "off", "no"] {
            assert_eq!(
                mode_from_values(Some(v), false),
                ToolSearchMode::Standard,
                "{v:?} should disable"
            );
        }
    }

    #[test]
    fn experimental_betas_kill_switch_forces_standard() {
        // Even an explicit enable is overridden by the kill switch.
        assert_eq!(
            mode_from_values(Some("true"), true),
            ToolSearchMode::Standard
        );
        assert_eq!(
            mode_from_values(Some("tst"), true),
            ToolSearchMode::Standard
        );
    }

    #[test]
    fn disabled_state_never_defers() {
        let d = DeferralState::disabled();
        assert!(!d.is_enabled());
        assert!(!d.should_defer("Task", true));
    }

    #[test]
    fn enabled_defers_should_defer_tools_only() {
        let d = DeferralState::new(ToolSearchMode::Enabled, false);
        assert!(d.should_defer("Task", true));
        assert!(!d.should_defer("Read", false));
    }

    #[test]
    fn loaded_tool_is_no_longer_deferred() {
        let d = DeferralState::new(ToolSearchMode::Enabled, false);
        assert!(d.should_defer("Task", true));
        d.mark_loaded(["Task".to_string()]);
        assert!(d.is_loaded("Task"));
        assert!(!d.should_defer("Task", true));
        assert!(d.wants_defer("Task", true));
    }

    #[test]
    fn unsupported_request_temporarily_disables_deferral() {
        let d = DeferralState::new(ToolSearchMode::Enabled, false);
        d.set_request_supported(false);
        assert!(!d.is_enabled());
        assert!(!d.should_defer("Task", true));
        d.set_request_supported(true);
        assert!(d.is_enabled());
    }

    #[test]
    fn loaded_tool_names_are_sorted_and_deduplicated() {
        let d = DeferralState::new(ToolSearchMode::Enabled, false);
        assert!(
            d.loaded_tool_names().is_empty(),
            "empty until a tool is loaded"
        );
        d.mark_loaded(["Zed", "Read", "Zed"]);
        assert_eq!(d.loaded_tool_names(), vec!["Read", "Zed"]);
        d.replace_loaded(["Task", "Task"]);
        assert_eq!(d.loaded_tool_names(), vec!["Task"]);
    }

    #[test]
    fn enter_worktree_not_deferred_in_bg_session() {
        // bg session: EnterWorktree is exempt from deferral even with wants_defer.
        let bg = DeferralState::new(ToolSearchMode::Enabled, true);
        assert!(!bg.should_defer(ENTER_WORKTREE_TOOL_NAME, true));
        // A different tool with wants_defer is still deferred in bg.
        assert!(bg.should_defer("Task", true));
        // Non-bg session: EnterWorktree follows the normal predicate.
        let fg = DeferralState::new(ToolSearchMode::Enabled, false);
        assert!(fg.should_defer(ENTER_WORKTREE_TOOL_NAME, true));
    }

    /// gap218 #10 (cc 2.1.218 `deferred_tools_delta`) — the announce → unchanged →
    /// remove → re-add state machine, with the BYTE-EXACT model-visible
    /// `<system-reminder>` bodies (extracted from the 2.1.218 binary at offset
    /// ~116526080). A DELTA is emitted only when the searchable set CHANGES; an
    /// unchanged step yields `None` (no full-catalog repeat).
    #[test]
    fn deferred_delta_announce_unchanged_remove_readd_bodies() {
        let d = DeferralState::new(ToolSearchMode::Enabled, false);

        // (1) First announcement: both names are genuinely new (addedLines).
        let delta = d.compute_deferred_delta(&["Alpha".to_string(), "Beta".to_string()]);
        assert_eq!(
            delta.added_lines,
            vec!["Alpha".to_string(), "Beta".to_string()]
        );
        assert!(delta.readded.is_empty() && delta.removed.is_empty());
        assert_eq!(
            delta.render_reminder().expect("announce reminder"),
            "<system-reminder>\nThe following deferred tools are now available via ToolSearch. Their schemas are NOT loaded \u{2014} calling them directly will fail with InputValidationError. Use ToolSearch with query \"select:<name>[,<name>...]\" to load tool schemas before calling them:\nAlpha\nBeta\n</system-reminder>"
        );

        // (2) Same set on the next step: empty delta ⇒ NO reminder.
        let delta = d.compute_deferred_delta(&["Alpha".to_string(), "Beta".to_string()]);
        assert!(delta.is_empty());
        assert_eq!(delta.render_reminder(), None);

        // (3) Beta disappears (MCP server disconnected): removed + ambient trailer.
        let delta = d.compute_deferred_delta(&["Alpha".to_string()]);
        assert_eq!(delta.removed, vec!["Beta".to_string()]);
        assert!(delta.added_lines.is_empty() && delta.readded.is_empty());
        assert_eq!(
            delta.render_reminder().expect("removed reminder"),
            "<system-reminder>\nThe following deferred tools are no longer available (their MCP server disconnected). Do not search for them \u{2014} ToolSearch will return no match:\nBeta\n\nThis is ambient context \u{2014} do not narrate it to the user unless they ask or it is directly relevant to their request.\n</system-reminder>"
        );

        // (4) Beta returns (server reconnected): announced earlier ⇒ readded, not new.
        let delta = d.compute_deferred_delta(&["Alpha".to_string(), "Beta".to_string()]);
        assert_eq!(delta.readded, vec!["Beta".to_string()]);
        assert!(delta.added_lines.is_empty() && delta.removed.is_empty());
        assert_eq!(
            delta.render_reminder().expect("readded reminder"),
            "<system-reminder>\n1 deferred tool is available again (MCP server reconnected \u{2014} names announced earlier in this conversation): Beta. Load via ToolSearch as before.\n</system-reminder>"
        );
    }

    /// gap218 #10 — `AIo` name grouping: `mcp__<server>__<tool>` names collapse to
    /// a single `mcp__<server>__* (n)` bucket in the re-add notice, and the plural
    /// "tools ... are" verb agrees with the raw name count.
    #[test]
    fn deferred_delta_groups_mcp_names_on_readd() {
        let d = DeferralState::new(ToolSearchMode::Enabled, false);
        let _ = d.compute_deferred_delta(&["mcp__srv__a".to_string(), "mcp__srv__b".to_string()]);
        let _ = d.compute_deferred_delta(&[]); // both removed
        let delta =
            d.compute_deferred_delta(&["mcp__srv__a".to_string(), "mcp__srv__b".to_string()]);
        assert_eq!(
            delta.readded,
            vec!["mcp__srv__a".to_string(), "mcp__srv__b".to_string()]
        );
        assert_eq!(
            delta.render_reminder().expect("readded reminder"),
            "<system-reminder>\n2 deferred tools are available again (MCP server reconnected \u{2014} names announced earlier in this conversation): mcp__srv__* (2). Load via ToolSearch as before.\n</system-reminder>"
        );
    }

    /// gap218 #10 — a name DISCOVERED via ToolSearch (loaded) leaves the searchable
    /// view but stays in the full tool set, so the oracle does NOT report it
    /// removed. The delta must be empty (no spurious "no longer available").
    #[test]
    fn deferred_delta_discovered_tool_is_not_reported_removed() {
        let d = DeferralState::new(ToolSearchMode::Enabled, false);
        let _ = d.compute_deferred_delta(&["Alpha".to_string(), "Beta".to_string()]);
        d.mark_loaded(["Beta"]);
        // Beta is now loaded, so the currently-deferred set is just Alpha.
        let delta = d.compute_deferred_delta(&["Alpha".to_string()]);
        assert!(
            delta.removed.is_empty(),
            "a discovered tool is not 'removed'"
        );
        assert!(delta.is_empty());
        assert_eq!(delta.render_reminder(), None);
    }
}
