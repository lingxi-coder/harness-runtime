//! Subagent-spawn permission-mode resolution — claude-code 2.1.207 `wKe`/`zol`
//! plus the runAgent context-override `ye=wKe(x,z); ve=ye??e.permissionMode`.
//!
//! The Agent tool's `mode` field ("plan"/"acceptEdits"/…) is CLAMPED against the
//! spawning parent's live permission mode so a child can never ESCALATE its own
//! privilege above the parent's, then the clamped value (or, failing that, the
//! agent definition's own permission mode) becomes the child's effective
//! permission-context mode. In claude-code every permission check the child runs
//! consults `toolPermissionContext.mode = ve`; in LingXi that override is threaded
//! into the subagent tool-dispatch permission gate (see
//! [`lingxi_core::host::tool_invoker::SubagentInvocationContext::mode_override`] →
//! [`lingxi_core::host::permission_gate::PermissionCheckContext::mode_override`]).
//!
//! `AgentPermissionMode` (`definition.rs`) is re-exported for callers that build
//! agent definitions.

use crate::definition::AgentPermissionMode;
use permission::PermissionMode;

/// Rank of a permission mode — claude-code `zol`
/// (`{plan:0,bubble:1,default:1,dontAsk:1,acceptEdits:2,auto:3,bypassPermissions:4}`).
/// A LOWER rank is MORE restrictive; a child may only request a mode whose rank
/// is `<=` the parent's (it can never escalate). `plan` (0) is therefore always
/// honored; `bypassPermissions` (4) only under a bypass parent.
fn mode_rank(mode: PermissionMode) -> u8 {
    match mode {
        PermissionMode::Plan => 0,
        PermissionMode::Bubble | PermissionMode::Default | PermissionMode::DontAsk => 1,
        PermissionMode::AcceptEdits => 2,
        PermissionMode::Auto => 3,
        PermissionMode::BypassPermissions => 4,
    }
}

/// Parse a spawn-mode WIRE string — the Agent tool `mode` enum
/// (`acceptEdits`/`auto`/`bypassPermissions`/`default`/`dontAsk`/`plan`) — into a
/// [`PermissionMode`]. Returns `None` for an unrecognized string (the schema enum
/// forbids one, so this is defensive). `bubble` is engine-internal and never
/// appears on the wire.
///
/// Retained (and unit-tested) for back-compat even though its production call
/// site was removed in 2.1.212: the Agent/Task `mode` call param is now
/// DEPRECATED and ignored, so the spawner no longer parses it into an override.
#[must_use]
#[allow(dead_code)]
pub(crate) fn parse_wire_mode(s: &str) -> Option<PermissionMode> {
    match s {
        "default" => Some(PermissionMode::Default),
        "plan" => Some(PermissionMode::Plan),
        "acceptEdits" => Some(PermissionMode::AcceptEdits),
        "bypassPermissions" => Some(PermissionMode::BypassPermissions),
        "dontAsk" => Some(PermissionMode::DontAsk),
        "auto" => Some(PermissionMode::Auto),
        _ => None,
    }
}

/// Canonical wire string for a [`PermissionMode`] — the inverse of
/// [`parse_wire_mode`] (extended with `bubble` for completeness, though it never
/// reaches the wire).
#[must_use]
pub(crate) fn wire_mode_str(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::Plan => "plan",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::BypassPermissions => "bypassPermissions",
        PermissionMode::DontAsk => "dontAsk",
        PermissionMode::Auto => "auto",
        PermissionMode::Bubble => "bubble",
    }
}

/// Main-loop/subagent override implied by an agent definition's own
/// `permissionMode` frontmatter.
#[must_use]
pub fn definition_mode_override(mode: AgentPermissionMode) -> Option<PermissionMode> {
    match mode {
        AgentPermissionMode::Bubble | AgentPermissionMode::Isolated => None,
        AgentPermissionMode::Default => Some(PermissionMode::Default),
        AgentPermissionMode::AcceptEdits => Some(PermissionMode::AcceptEdits),
        AgentPermissionMode::DontAsk => Some(PermissionMode::DontAsk),
        AgentPermissionMode::BypassPermissions => Some(PermissionMode::BypassPermissions),
        AgentPermissionMode::Auto => Some(PermissionMode::Auto),
        AgentPermissionMode::Plan => Some(PermissionMode::Plan),
    }
}

/// The three live inputs to claude's spawn-time `bypassPermissions` clamps
/// (runAgent `bs(Rn)`), threaded as DATA rather than read from process globals
/// inside [`effective_child_mode`].
///
/// 🚨 `CLAUDE_CODE_EVAL_CONFINED` is a PROCESS global. A gate that reads it while
/// some test in the same binary `set_var`s it makes the whole parallel suite
/// flaky — exactly the bug that had to be undone in
/// `permission::PermissionPolicy::from_rules`. Read each input ONCE at the
/// composition root and pass the answer here.
///
/// The default (all `false`) is the pre-2.1.263 behavior: no clamp fires.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpawnBypassGates {
    /// claude `YYe()` — the session is a confined evaluation run
    /// (`CLAUDE_CODE_EVAL_CONFINED === "true"`, compared against that LITERAL).
    /// Same predicate as `permission`'s `OG` allow-rule filter and `hooks`' `H_n`
    /// hook-allow suppression; this is its third consumer.
    pub confined: bool,
    /// claude `ey()`, which is `pAn() !== void 0` where
    /// `pAn(){if((bn()||{}).permissions?.disableBypassPermissionsMode==="disable")
    /// return "Bypass permissions mode was disabled by settings";return}` — i.e.
    /// settings `permissions.disableBypassPermissionsMode === "disable"`.
    ///
    /// 🚨 The refusal COPY says "not running in a contained no-internet
    /// environment", but that is prose only: the oracle's middle disjunct is the
    /// constant-folded `!1` (dead in this build), so the two LIVE conditions are
    /// this settings bit and [`Self::restricted`]. Do not implement a
    /// network-containment probe on the strength of the message text.
    pub bypass_disabled: bool,
    /// claude `Rn.restricted` — the 2.1.251 `--restricted` session bit.
    pub restricted: bool,
}

/// Clamp a REQUESTED child spawn mode against the PARENT's live mode — claude
/// `wKe(e,t)` with `e`=requested, `t`=parent:
///
/// - no requested mode ⇒ `None` (`if(!e)return`);
/// - parent `auto` + requested `acceptEdits` ⇒ `None` (the special-case drop,
///   `if(t==="auto"&&e==="acceptEdits")return`);
/// - otherwise honor the requested mode only when `zol[requested] <= zol[parent]`
///   (`return zol[e]<=zol[t]?e:void 0`) — the child cannot escalate above the
///   parent.
#[must_use]
fn clamp_spawn_mode(
    requested: Option<PermissionMode>,
    parent: PermissionMode,
) -> Option<PermissionMode> {
    let requested = requested?;
    if parent == PermissionMode::Auto && requested == PermissionMode::AcceptEdits {
        return None;
    }
    (mode_rank(requested) <= mode_rank(parent)).then_some(requested)
}

/// The [`PermissionMode`] fallback for a subagent definition's own
/// [`AgentPermissionMode`], used as claude's `ve = ye ?? e.permissionMode`.
///
/// Only `Plan`/`Auto` carry a permission-check semantics worth applying as an
/// override; `Bubble` (LingXi's default, ≈ claude's *undefined* frontmatter
/// `permissionMode`) and `Isolated` (no claude mode equivalent) map to `None` so
/// the overwhelmingly common case leaves the child inheriting the parent/live
/// mode unchanged — byte-identical to pre-2.1.207.
#[must_use]
fn definition_mode_fallback(mode: AgentPermissionMode) -> Option<PermissionMode> {
    definition_mode_override(mode)
}

/// Compute a spawned child's EFFECTIVE permission-context mode override — claude
/// runAgent's `ye=wKe(x,z); ve=ye??e.permissionMode` plus the
/// `getToolPermissionContext` guard
/// `if(ve&&(ye||ht.mode!=="bypassPermissions"&&ht.mode!=="acceptEdits"&&ht.mode!=="auto"))`.
///
/// Returns `Some(mode)` when the child's permission checks must run under an
/// OVERRIDE mode (threaded into the dispatch permission gate), or `None` when the
/// child simply inherits the parent/live mode (no override).
///
/// followed by the TWO inner clamps of that same `bs(Rn)` body (2.1.263):
///
/// ```js
/// let ys = So;
/// if (YYe() && !as && (So==="bypassPermissions"||So==="acceptEdits"||So==="auto"))
///   n(`Subagent declared permissionMode: ${So} inside a confined evaluation run; keeping parent mode '${Rn.mode}'.`,{level:"warn"}), ys = Rn.mode;
/// else if (So === "bypassPermissions") {
///   let ws = ey(), _s = !1;
///   if (ws || !1 || Rn.restricted)
///     n(`Subagent declared permissionMode: bypassPermissions but this session is not running in a contained no-internet environment (or bypass is policy-disabled, or the session is --restricted); keeping parent mode '${Rn.mode}'.`,{level:"warn"}), ys = Rn.mode;
/// }
/// Ar = {...Ar, mode: ys};
/// ```
///
/// Without these an agent-definition frontmatter line `permissionMode:
/// bypassPermissions` ESCALATES a `default` parent session to full bypass —
/// the outer guard only suppresses the definition fallback under an ALREADY
/// permissive parent, so a restrictive parent is exactly the case it lets
/// through.
///
/// A clamped spawn keeps the PARENT's mode rather than returning `None`: the
/// oracle assigns `ys = Rn.mode` and still applies it (`Ar={...Ar,mode:ys}`), so
/// the child runs under an override that EQUALS the live parent anchor.
///
/// - `requested` — the parsed spawn `mode` (`None` when the caller omitted it).
/// - `parent` — the spawner's live permission-mode anchor (≈ claude's `ht.mode`).
/// - `def_mode` — the resolved agent definition's own permission mode.
/// - `gates` — see [`SpawnBypassGates`]; `Default::default()` disables both clamps.
/// - `warn` — the `n(…,{level:"warn"})` sink (production: `tracing::warn!`).
#[must_use]
pub(crate) fn effective_child_mode(
    requested: Option<PermissionMode>,
    parent: PermissionMode,
    def_mode: AgentPermissionMode,
    gates: SpawnBypassGates,
    warn: &mut dyn FnMut(&str),
) -> Option<PermissionMode> {
    let clamped = clamp_spawn_mode(requested, parent); // `ye`
    let effective = clamped.or_else(|| definition_mode_fallback(def_mode))?; // `ve`
                                                                             // Context-override guard: apply `ve` UNLESS it came only from the definition
                                                                             // fallback (no explicit clamped spawn mode) while the parent is already a
                                                                             // permissive mode the fallback must not silently downgrade.
    if clamped.is_none()
        && matches!(
            parent,
            PermissionMode::BypassPermissions | PermissionMode::AcceptEdits | PermissionMode::Auto
        )
    {
        return None;
    }
    // `if (YYe() && !as && So ∈ {bypassPermissions, acceptEdits, auto})`. The
    // outer guard above already returned for a permissive parent, so reaching
    // here with `clamped.is_none()` means a RESTRICTIVE parent is about to be
    // raised by the definition alone — the escalation a confined run must refuse.
    if gates.confined
        && clamped.is_none()
        && matches!(
            effective,
            PermissionMode::BypassPermissions | PermissionMode::AcceptEdits | PermissionMode::Auto
        )
    {
        warn(&format!(
            "Subagent declared permissionMode: {} inside a confined evaluation run; keeping parent mode '{}'.",
            wire_mode_str(effective),
            wire_mode_str(parent)
        ));
        return Some(parent);
    }
    // `else if (So === "bypassPermissions") { if (ey() || !1 || Rn.restricted) }`.
    // Reached only when the confined arm did not fire (the oracle's `else`).
    if effective == PermissionMode::BypassPermissions && (gates.bypass_disabled || gates.restricted)
    {
        warn(&format!(
            "Subagent declared permissionMode: bypassPermissions but this session is not \
             running in a contained no-internet environment (or bypass is policy-disabled, \
             or the session is --restricted); keeping parent mode '{}'.",
            wire_mode_str(parent)
        ));
        return Some(parent);
    }
    Some(effective)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-2.1.263 call shape: no clamps armed, warnings discarded. Keeps the
    /// existing `ve` cases readable while the two new clamps get their own tests.
    fn ecm(
        requested: Option<PermissionMode>,
        parent: PermissionMode,
        def_mode: AgentPermissionMode,
    ) -> Option<PermissionMode> {
        effective_child_mode(
            requested,
            parent,
            def_mode,
            SpawnBypassGates::default(),
            &mut |_| {},
        )
    }

    /// Same, but with `gates` armed; returns the mode AND every warning emitted.
    fn ecm_gated(
        requested: Option<PermissionMode>,
        parent: PermissionMode,
        def_mode: AgentPermissionMode,
        gates: SpawnBypassGates,
    ) -> (Option<PermissionMode>, Vec<String>) {
        let mut warnings = Vec::new();
        let mode = effective_child_mode(requested, parent, def_mode, gates, &mut |m| {
            warnings.push(m.to_string());
        });
        (mode, warnings)
    }

    // ── wKe clamp semantics (claude 2.1.207 `wKe`/`zol`) ──────────────────────

    #[test]
    fn clamp_none_requested_is_none() {
        assert_eq!(clamp_spawn_mode(None, PermissionMode::Default), None);
    }

    #[test]
    fn clamp_child_cannot_escalate_above_parent() {
        // parent default (rank 1) + requested bypassPermissions (rank 4) → dropped.
        assert_eq!(
            clamp_spawn_mode(
                Some(PermissionMode::BypassPermissions),
                PermissionMode::Default
            ),
            None
        );
        // parent default (1) + requested acceptEdits (2) → dropped.
        assert_eq!(
            clamp_spawn_mode(Some(PermissionMode::AcceptEdits), PermissionMode::Default),
            None
        );
        // parent default (1) + requested auto (3) → dropped.
        assert_eq!(
            clamp_spawn_mode(Some(PermissionMode::Auto), PermissionMode::Default),
            None
        );
    }

    #[test]
    fn clamp_auto_parent_drops_requested_accept_edits() {
        // Special-case: parent auto + requested acceptEdits → dropped even though
        // rank 2 <= rank 3.
        assert_eq!(
            clamp_spawn_mode(Some(PermissionMode::AcceptEdits), PermissionMode::Auto),
            None
        );
        // But a different requested mode <= auto is still honored.
        assert_eq!(
            clamp_spawn_mode(Some(PermissionMode::Default), PermissionMode::Auto),
            Some(PermissionMode::Default)
        );
    }

    #[test]
    fn clamp_plan_is_honored_under_every_parent() {
        for parent in [
            PermissionMode::Default,
            PermissionMode::Plan,
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
            PermissionMode::BypassPermissions,
            PermissionMode::DontAsk,
            PermissionMode::Bubble,
        ] {
            assert_eq!(
                clamp_spawn_mode(Some(PermissionMode::Plan), parent),
                Some(PermissionMode::Plan),
                "plan (rank 0) must be honored under parent {parent:?}"
            );
        }
    }

    #[test]
    fn clamp_equal_or_lower_rank_is_honored() {
        // parent acceptEdits (2) + requested default (1) → honored (downgrade ok).
        assert_eq!(
            clamp_spawn_mode(Some(PermissionMode::Default), PermissionMode::AcceptEdits),
            Some(PermissionMode::Default)
        );
        // parent bypass (4) + requested bypass (4) → honored (equal rank).
        assert_eq!(
            clamp_spawn_mode(
                Some(PermissionMode::BypassPermissions),
                PermissionMode::BypassPermissions
            ),
            Some(PermissionMode::BypassPermissions)
        );
    }

    // ── ve computation + context-override guard ───────────────────────────────

    #[test]
    fn explicit_spawn_mode_wins_over_definition() {
        // Requested plan, definition Bubble, any parent → plan (clamp keeps plan).
        assert_eq!(
            ecm(
                Some(PermissionMode::Plan),
                PermissionMode::BypassPermissions,
                AgentPermissionMode::Bubble
            ),
            Some(PermissionMode::Plan)
        );
    }

    #[test]
    fn definition_plan_fallback_applies_under_non_permissive_parent() {
        // No spawn mode, definition Plan, parent default → the definition fallback
        // supplies Plan.
        assert_eq!(
            ecm(None, PermissionMode::Default, AgentPermissionMode::Plan),
            Some(PermissionMode::Plan)
        );
    }

    #[test]
    fn definition_fallback_suppressed_under_permissive_parent() {
        // No spawn mode + definition Plan, but the parent is already permissive
        // (bypassPermissions/acceptEdits/auto) → the fallback is SUPPRESSED so the
        // permissive parent is not silently downgraded (claude's `ht.mode` guard).
        for parent in [
            PermissionMode::BypassPermissions,
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
        ] {
            assert_eq!(
                ecm(None, parent, AgentPermissionMode::Plan),
                None,
                "definition fallback must be suppressed under permissive parent {parent:?}"
            );
        }
    }

    #[test]
    fn explicit_spawn_mode_not_suppressed_under_permissive_parent() {
        // An EXPLICIT clamped spawn mode is applied even under a permissive parent
        // (the guard only suppresses the definition-only fallback).
        assert_eq!(
            ecm(
                Some(PermissionMode::Plan),
                PermissionMode::Auto,
                AgentPermissionMode::Bubble
            ),
            Some(PermissionMode::Plan)
        );
    }

    #[test]
    fn bubble_definition_default_yields_no_override() {
        // The common case: no spawn mode, Bubble definition (LingXi default),
        // parent default → no override (child inherits the live mode).
        assert_eq!(
            ecm(None, PermissionMode::Default, AgentPermissionMode::Bubble),
            None
        );
    }

    #[test]
    fn escalating_spawn_mode_falls_back_to_definition() {
        // Requested bypass under a default parent is dropped by the clamp; with a
        // Plan definition the fallback still yields Plan (never the escalation).
        assert_eq!(
            ecm(
                Some(PermissionMode::BypassPermissions),
                PermissionMode::Default,
                AgentPermissionMode::Plan
            ),
            Some(PermissionMode::Plan)
        );
    }

    // ── the two 2.1.263 inner clamps (`bs(Rn)`) ──────────────────────────────

    /// THE ESCALATION the confined clamp exists to stop: frontmatter alone
    /// raising a restrictive parent. Without the clamp this returns
    /// `Some(BypassPermissions)`.
    #[test]
    fn confined_run_refuses_a_definition_declared_escalation() {
        let gates = SpawnBypassGates {
            confined: true,
            ..SpawnBypassGates::default()
        };
        for (def_mode, wire) in [
            (AgentPermissionMode::BypassPermissions, "bypassPermissions"),
            (AgentPermissionMode::AcceptEdits, "acceptEdits"),
            (AgentPermissionMode::Auto, "auto"),
        ] {
            for parent in [
                PermissionMode::Default,
                PermissionMode::DontAsk,
                PermissionMode::Plan,
            ] {
                let (mode, warnings) = ecm_gated(None, parent, def_mode, gates);
                assert_eq!(
                    mode,
                    Some(parent),
                    "confined run must keep parent {parent:?} against definition {def_mode:?}"
                );
                assert_eq!(warnings.len(), 1, "exactly one warning for {def_mode:?}");
                assert_eq!(
                    warnings[0],
                    format!(
                        "Subagent declared permissionMode: {wire} inside a confined \
                         evaluation run; keeping parent mode '{}'.",
                        wire_mode_str(parent)
                    )
                );
            }
        }
    }

    /// The clamp is armed by the FLAG, not by the mode: unconfined, the same
    /// definition still escalates (this is what makes the test above meaningful).
    #[test]
    fn an_unconfined_run_still_applies_the_definition_mode() {
        let (mode, warnings) = ecm_gated(
            None,
            PermissionMode::Default,
            AgentPermissionMode::BypassPermissions,
            SpawnBypassGates::default(),
        );
        assert_eq!(mode, Some(PermissionMode::BypassPermissions));
        assert!(warnings.is_empty());
    }

    /// `!as` in the oracle: an EXPLICIT clamped spawn mode is exempt from the
    /// confined arm. It cannot be an escalation anyway — `clamp_spawn_mode`
    /// already capped it at the parent's rank.
    #[test]
    fn confined_run_leaves_an_explicit_clamped_spawn_mode_alone() {
        let gates = SpawnBypassGates {
            confined: true,
            ..SpawnBypassGates::default()
        };
        let (mode, warnings) = ecm_gated(
            Some(PermissionMode::Auto),
            PermissionMode::BypassPermissions,
            AgentPermissionMode::Bubble,
            gates,
        );
        assert_eq!(mode, Some(PermissionMode::Auto));
        assert!(warnings.is_empty(), "explicit spawn mode must not warn");
    }

    /// `ey()` (settings `disableBypassPermissionsMode: "disable"`) and
    /// `Rn.restricted` each independently refuse a declared bypass.
    #[test]
    fn bypass_is_refused_when_disabled_by_settings_or_restricted() {
        for gates in [
            SpawnBypassGates {
                bypass_disabled: true,
                ..SpawnBypassGates::default()
            },
            SpawnBypassGates {
                restricted: true,
                ..SpawnBypassGates::default()
            },
        ] {
            let (mode, warnings) = ecm_gated(
                None,
                PermissionMode::Default,
                AgentPermissionMode::BypassPermissions,
                gates,
            );
            assert_eq!(
                mode,
                Some(PermissionMode::Default),
                "bypass must be refused under {gates:?}"
            );
            assert_eq!(warnings.len(), 1);
            assert_eq!(
                warnings[0],
                "Subagent declared permissionMode: bypassPermissions but this session is not \
                 running in a contained no-internet environment (or bypass is policy-disabled, \
                 or the session is --restricted); keeping parent mode 'default'."
            );
        }
    }

    /// The bypass arm is bypass-ONLY: `acceptEdits`/`auto` definitions are
    /// untouched by the settings/restricted bits (only the confined arm covers
    /// them). Writing one shared clamp for all three would be green on the test
    /// above and wrong here.
    #[test]
    fn the_bypass_arm_does_not_clamp_accept_edits_or_auto() {
        let gates = SpawnBypassGates {
            bypass_disabled: true,
            restricted: true,
            ..SpawnBypassGates::default()
        };
        for (def_mode, expected) in [
            (
                AgentPermissionMode::AcceptEdits,
                PermissionMode::AcceptEdits,
            ),
            (AgentPermissionMode::Auto, PermissionMode::Auto),
        ] {
            let (mode, warnings) = ecm_gated(None, PermissionMode::Default, def_mode, gates);
            assert_eq!(
                mode,
                Some(expected),
                "{def_mode:?} must survive the bypass arm"
            );
            assert!(warnings.is_empty());
        }
    }

    /// The oracle's `else if`: when the confined arm fires for a declared bypass,
    /// the bypass arm must NOT also fire — one warning, not two.
    #[test]
    fn confined_arm_shadows_the_bypass_arm() {
        let gates = SpawnBypassGates {
            confined: true,
            bypass_disabled: true,
            restricted: true,
        };
        let (mode, warnings) = ecm_gated(
            None,
            PermissionMode::Default,
            AgentPermissionMode::BypassPermissions,
            gates,
        );
        assert_eq!(mode, Some(PermissionMode::Default));
        assert_eq!(warnings.len(), 1, "the arms are exclusive (`else if`)");
        assert!(
            warnings[0].contains("inside a confined evaluation run"),
            "the CONFINED copy must win, got: {}",
            warnings[0]
        );
    }

    /// A permissive parent is still handled by the OUTER guard (returns `None`,
    /// no override, no warning) — the new clamps must not have changed that.
    #[test]
    fn clamps_do_not_disturb_the_permissive_parent_guard() {
        let gates = SpawnBypassGates {
            confined: true,
            bypass_disabled: true,
            restricted: true,
        };
        for parent in [
            PermissionMode::BypassPermissions,
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
        ] {
            let (mode, warnings) =
                ecm_gated(None, parent, AgentPermissionMode::BypassPermissions, gates);
            assert_eq!(
                mode, None,
                "outer guard must still win for parent {parent:?}"
            );
            assert!(warnings.is_empty());
        }
    }

    #[test]
    fn definition_modes_round_trip_to_permission_modes() {
        assert_eq!(
            definition_mode_override(AgentPermissionMode::Default),
            Some(PermissionMode::Default)
        );
        assert_eq!(
            definition_mode_override(AgentPermissionMode::AcceptEdits),
            Some(PermissionMode::AcceptEdits)
        );
        assert_eq!(
            definition_mode_override(AgentPermissionMode::DontAsk),
            Some(PermissionMode::DontAsk)
        );
        assert_eq!(
            definition_mode_override(AgentPermissionMode::BypassPermissions),
            Some(PermissionMode::BypassPermissions)
        );
        assert_eq!(
            definition_mode_override(AgentPermissionMode::Plan),
            Some(PermissionMode::Plan)
        );
    }

    #[test]
    fn wire_mode_round_trips() {
        for (s, m) in [
            ("default", PermissionMode::Default),
            ("plan", PermissionMode::Plan),
            ("acceptEdits", PermissionMode::AcceptEdits),
            ("bypassPermissions", PermissionMode::BypassPermissions),
            ("dontAsk", PermissionMode::DontAsk),
            ("auto", PermissionMode::Auto),
        ] {
            assert_eq!(parse_wire_mode(s), Some(m));
            assert_eq!(wire_mode_str(m), s);
        }
        assert_eq!(parse_wire_mode("nonsense"), None);
    }
}
