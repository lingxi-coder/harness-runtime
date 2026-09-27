//! The `Artifact` tool enablement gate, shared by the tool itself and by
//! WebFetch's prompt (which surfaces a "claude.ai/code/artifact URLs are
//! fetchable" exception only when the Artifact tool is live).
//!
//! Port of claude-code 2.1.207 `dY()` (binary offset ~; parity H-BIN-03) and
//! its helper chain. The tool is **register-but-disable**: with no Statsig
//! backend wired the dominant gate `tengu_cobalt_plinth` reads its code-default
//! `false`, so `is_enabled()` returns `false` and the tool is invisible to the
//! model — byte-identical to the shipped binary on a host without the
//! `cobalt_plinth` gate (and, per the binary's own `enableArtifact` schema,
//! "enabled once the feature is available" only for first-party claude.ai
//! accounts with the right subscription tier).
//!
//! ```text
//! dY()  = x9i() && k9i() && (P7t() ?? L7t())
//! x9i() = !R9i() && qXc()
//! R9i() = env CLAUDE_CODE_DISABLE_ARTIFACT || settings.disableArtifact === true
//! qXc() = !BXc() && UXc()
//! BXc() = $Xc(entrypoint): "local-agent" | "claude-coworker*"
//! UXc() = $o() && xn()==="firstParty" && !ha() && !ou(CLAUDE_CODE_ARTIFACT)
//!         && !(!ct(CLAUDE_CODE_ARTIFACT) && FXc())
//! FXc() = entrypoint ∈ {claude-code-github-action, mcp, sdk-ts/py/cli, …}
//! k9i() = Qe("tengu_cobalt_plinth", false) && WXc()
//! WXc() = subscriptionType ∈ {team,enterprise,pro,max,null}
//!         && Xi("allow_cobalt_plinth")
//! P7t() = the `enableArtifact` setting from the config scopes, else undefined
//! L7t() = true
//! ```
//!
//! **Wired here:** the two Statsig gates (`tengu_cobalt_plinth`,
//! `allow_cobalt_plinth`, both code-default `false`), the `R9i()` env disable
//! (`CLAUDE_CODE_DISABLE_ARTIFACT`), the `BXc()`/`FXc()` entrypoint exclusions,
//! and the `CLAUDE_CODE_ARTIFACT` env override (`ou`/`ct`).
//!
//! **Stage-2 seams (documented, default permissive — the flag gate above
//! already keeps the tool disabled in this build):** the live first-party
//! OAuth-account + auth-route detection (`$o()`/`xn()`), the subscription-tier
//! read (`Us()`), and the `settings.disableArtifact`/`enableArtifact` runtime
//! consumption (`R9i()` settings half + `P7t()`). The settings KEYS round-trip
//! via `lingxi_core::settings::SettingsJson`; wiring their live values into this gate
//! lands with the Stage-2 publish/list pipeline.

use platform_api::env::{is_env_defined_falsy, is_env_truthy};

/// Binary `dw` — the Artifact tool's wire name.
pub const ARTIFACT_TOOL_NAME: &str = "Artifact";

/// Binary `Qe("tengu_cobalt_plinth", false)` — the dominant Statsig gate
/// (code-default `false`; with no flag backend the tool stays disabled).
const COBALT_PLINTH_FLAG: &str = "tengu_cobalt_plinth";

/// Binary `Xi("allow_cobalt_plinth")` — the second remote gate inside `WXc()`.
const ALLOW_COBALT_PLINTH_GATE: &str = "allow_cobalt_plinth";

/// `dY()` — is the Artifact tool enabled? See module docs for the full chain.
///
/// Returns `false` by default in this build (no Statsig backend ⇒
/// `tengu_cobalt_plinth` reads its code-default `false`), so the tool registers
/// disabled and is invisible to the model — byte-identical to the shipped
/// binary on a host without the `cobalt_plinth` gate.
#[must_use]
pub fn is_enabled() -> bool {
    // dY() = x9i() && k9i() && (P7t() ?? L7t())
    if !x9i() {
        return false;
    }
    if !k9i() {
        return false;
    }
    // P7t() ?? L7t(): the `enableArtifact` setting, else L7t() (=true).
    enable_artifact_setting().unwrap_or(true)
}

/// `x9i()` = `!R9i() && qXc()`.
fn x9i() -> bool {
    !disabled_by_config() && qxc()
}

/// `R9i()` = `env CLAUDE_CODE_DISABLE_ARTIFACT || settings.disableArtifact===true`.
/// The env half is wired; the `settings.disableArtifact` half is a Stage-2 seam
/// (the schema key round-trips, but its live value is not yet threaded here — the
/// dominant Statsig gate already keeps the tool disabled in this build).
fn disabled_by_config() -> bool {
    is_env_truthy(
        std::env::var("CLAUDE_CODE_DISABLE_ARTIFACT")
            .ok()
            .as_deref(),
    )
}

/// `qXc()` = `!BXc() && UXc()`.
fn qxc() -> bool {
    !excluded_entrypoint() && first_party_available()
}

/// `BXc()` = `$Xc(CLAUDE_CODE_ENTRYPOINT)` — the `local-agent` and
/// `claude-coworker*` entrypoints are hard-excluded.
fn excluded_entrypoint() -> bool {
    match std::env::var("CLAUDE_CODE_ENTRYPOINT") {
        Ok(e) => e == "local-agent" || e.starts_with("claude-coworker"),
        Err(_) => false,
    }
}

/// `UXc()` — first-party availability: an OAuth (claude.ai) account on a
/// non-gateway/bedrock/foundry route, not a github-action/mcp/sdk entrypoint,
/// honoring the `CLAUDE_CODE_ARTIFACT` env override (`ou`/`ct`).
///
/// Stage-2 seam: the live `$o()` (OAuth-account + scopes) and `xn()==="firstParty"`
/// (auth-route) detection is not yet wired, so this defaults permissive on the
/// auth axis; the `tengu_cobalt_plinth` gate in [`k9i`] already keeps the tool
/// disabled in this build. The env override and entrypoint backstop ARE wired.
fn first_party_available() -> bool {
    // ou(CLAUDE_CODE_ARTIFACT): an explicitly-falsy override kills it.
    if is_env_defined_falsy(std::env::var("CLAUDE_CODE_ARTIFACT").ok().as_deref()) {
        return false;
    }
    // ct(CLAUDE_CODE_ARTIFACT): an explicitly-truthy override bypasses the
    // FXc() entrypoint backstop below.
    if is_env_truthy(std::env::var("CLAUDE_CODE_ARTIFACT").ok().as_deref()) {
        return true;
    }
    // !FXc(): github-action / mcp / sdk entrypoints are excluded.
    // (real $o()/xn() first-party-auth detection is Stage-2)
    !excluded_backstop_entrypoint()
}

/// `FXc()` — the github-action / mcp / sdk entrypoint backstop (the chat-relay
/// half of the binary `FXc()` is a separate remote gate, omitted here).
fn excluded_backstop_entrypoint() -> bool {
    match std::env::var("CLAUDE_CODE_ENTRYPOINT") {
        Ok(e) => {
            e == "claude-code-github-action"
                || e == "mcp"
                || e == "sdk-ts"
                || e == "sdk-py"
                || e == "sdk-cli"
        }
        Err(_) => false,
    }
}

/// `k9i()` = `Qe("tengu_cobalt_plinth", false) && WXc()`.
fn k9i() -> bool {
    telemetry::flag_bool(COBALT_PLINTH_FLAG, false) && wxc()
}

/// `WXc()` = `subscriptionType ∈ {team,enterprise,pro,max,null} && Xi("allow_cobalt_plinth")`.
///
/// Stage-2 seam: the live subscription-tier read (`Us()`) is not yet wired, so
/// the tier check defaults permissive; the `allow_cobalt_plinth` remote gate
/// (code-default `false`) is wired.
fn wxc() -> bool {
    subscription_tier_ok() && telemetry::flag_bool(ALLOW_COBALT_PLINTH_GATE, false)
}

/// Stage-2 seam for `WXc()`'s `Us()` subscription-tier read. Defaults `true`
/// (the flag gate in [`k9i`] already gates the tool off in this build).
fn subscription_tier_ok() -> bool {
    true
}

/// `P7t()` — the `enableArtifact` setting from the config scopes, else `None`
/// (⇒ `dY()` falls back to `L7t()` = `true`, "enabled once the feature is
/// available"). Stage-2 seam: live settings consumption lands with the publish
/// pipeline; the schema key already round-trips via `SettingsJson`.
fn enable_artifact_setting() -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize env + flag mutation across the gate tests (shared process env).
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CLAUDE_CODE_DISABLE_ARTIFACT");
        std::env::remove_var("CLAUDE_CODE_ARTIFACT");
        std::env::remove_var("CLAUDE_CODE_ENTRYPOINT");
        telemetry::test_clear_flag(COBALT_PLINTH_FLAG);
        telemetry::test_clear_flag(ALLOW_COBALT_PLINTH_GATE);
        g
    }

    fn enable_flags() {
        telemetry::test_set_flag(COBALT_PLINTH_FLAG, true);
        telemetry::test_set_flag(ALLOW_COBALT_PLINTH_GATE, true);
    }

    /// Default host (no Statsig backend): the dominant `tengu_cobalt_plinth`
    /// gate reads its code-default `false`, so the tool is DISABLED — the
    /// load-bearing parity (byte-identical to CC on a host without the gate).
    #[test]
    fn disabled_by_default() {
        let _g = guard();
        assert!(!is_enabled());
    }

    /// Either Statsig gate alone is insufficient — `k9i()` requires BOTH.
    #[test]
    fn one_flag_is_not_enough() {
        let _g = guard();
        telemetry::test_set_flag(COBALT_PLINTH_FLAG, true);
        assert!(!is_enabled());
        telemetry::test_clear_flag(COBALT_PLINTH_FLAG);
        telemetry::test_set_flag(ALLOW_COBALT_PLINTH_GATE, true);
        assert!(!is_enabled());
    }

    /// With both remote gates on (and the Stage-2 auth/tier seams permissive),
    /// the tool enables — proving the gate chain wires through.
    #[test]
    fn enabled_with_both_flags() {
        let _g = guard();
        enable_flags();
        assert!(is_enabled());
    }

    /// `R9i()` env disable overrides the flags.
    #[test]
    fn env_disable_wins() {
        let _g = guard();
        enable_flags();
        std::env::set_var("CLAUDE_CODE_DISABLE_ARTIFACT", "1");
        assert!(!is_enabled());
    }

    /// `BXc()` — the `local-agent` / `claude-coworker*` entrypoints are excluded
    /// even with the flags on.
    #[test]
    fn bxc_entrypoint_excluded() {
        let _g = guard();
        enable_flags();
        std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "local-agent");
        assert!(!is_enabled());
        std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "claude-coworker-abc");
        assert!(!is_enabled());
    }

    /// `FXc()` — github-action / mcp / sdk entrypoints are excluded, but a
    /// truthy `CLAUDE_CODE_ARTIFACT` override bypasses the backstop.
    #[test]
    fn fxc_backstop_and_env_override() {
        let _g = guard();
        enable_flags();
        std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "mcp");
        assert!(!is_enabled());
        // ct(CLAUDE_CODE_ARTIFACT) bypasses the FXc backstop.
        std::env::set_var("CLAUDE_CODE_ARTIFACT", "1");
        assert!(is_enabled());
        // ou(CLAUDE_CODE_ARTIFACT) kills it regardless.
        std::env::set_var("CLAUDE_CODE_ARTIFACT", "0");
        assert!(!is_enabled());
    }
}
