//! `bashCommandClamp` — the per-spawn shell-execution clamp (NEW in
//! claude-code 2.1.238; `bashCommandClamp` count 2.1.220 → 2.1.238 is 0 → 18).
//!
//! A clamp is a GROUP of `Bash(...)` allow-rule strings attached to an agent at
//! SPAWN time through the `bash_command_clamp` permission layer
//! ([`crate::layers::PermissionLayer::BashCommandClamp`]). While any group is
//! present the agent may run ONLY shell command forms that every group admits,
//! and every other shell surface (PowerShell, a `Monitor` WebSocket) is denied
//! outright — those surfaces cannot be expressed as Bash command forms, so they
//! can never satisfy the clamp.
//!
//! ## The four model-visible denials (all four are byte-locked here)
//!
//! | oracle | offset | ported as |
//! |---|---|---|
//! | `Jkf` | @290146703 | [`clamp_surface_deny_message`] |
//! | `gCi` | @290147095 | [`clamp_crash_deny_message`] |
//! | `M8n` | @290160634 | [`clamp_bash_deny_message`] |
//! | `Wfm` | @294394648 | [`POWERSHELL_CLAMP_DENY_MESSAGE`] |
//!
//! plus the two decision reasons `sMr` / `fxn` (binary @73808064):
//! [`CLAMP_NO_MATCH_REASON`] and [`CLAMP_FAIL_CLOSED_REASON`].
//!
//! ## Where these are CALLED
//!
//! * [`crate::PermissionPolicy::bash_command_clamp_deny`], reached from
//!   `authorize_inner` at the `checkPermissions` slot of the `mSm` precedence
//!   walk (after the tool-wide deny / content deny / tool-wide ask walks,
//!   before the content-ask walk) — the Bash, PowerShell and `Monitor{ws}`
//!   denials.
//! * [`crate::PolicyPermissionGate`]'s authorize wrapper, which converts a
//!   PANIC out of the rule engine into [`clamp_crash_deny_message`] instead of
//!   unwinding, but ONLY while a clamp is active (upstream `gCi` is a tool's
//!   `permissionCheckFailureDecision`, invoked by `wTv`/`FJa` @290295374).
//!
//! ## Inert in a default install
//!
//! Nothing in a stock LingXi session produces a `bash_command_clamp` layer:
//! upstream feeds it from the workflow `agent()` option `opts.bashCommandClamp`
//! (cc-238 @229230054), and this port's `workflow` crate has no `agent()`
//! options. Every gate below is therefore OFF today and has zero wire-byte
//! impact — but it is now correctly gated, so the moment a spawn seam attaches
//! a clamp layer the behavior matches upstream exactly.

use crate::shell_command::{command_exact_allowed, command_fully_allowed, split_command};

/// `sMr` (binary @73808064) — the decision reason on all three clamp denials.
pub const CLAMP_NO_MATCH_REASON: &str = "bashCommandClamp: no clamp rule matches this command";

/// `fxn` (binary @73808064) — the decision reason on the fail-closed crash deny.
pub const CLAMP_FAIL_CLOSED_REASON: &str = "bashCommandClamp fail-closed: permission check crashed";

/// `Wfm`'s PowerShell arm (cc-238 @294394648). Em-dash is U+2014.
pub const POWERSHELL_CLAMP_DENY_MESSAGE: &str = "Permission to use PowerShell has been denied: this agent carries a per-spawn bashCommandClamp, which scopes shell execution to a fixed set of Bash command forms \u{2014} PowerShell commands cannot match them. Use the clamped Bash forms instead.";

/// The tool name upstream passes to `Jkf` from the `Monitor` tool's WebSocket
/// arm (`Jkf("Monitor websocket", t)`, cc-238 @292829969).
pub const MONITOR_WEBSOCKET_SURFACE: &str = "Monitor websocket";

/// Why a command failed its clamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClampMissKind {
    /// `kind:"unverifiable"` — the command could not be decomposed into spans
    /// the clamp can be checked against (substitution, control flow, an
    /// undecomposable compound, or a parse abort).
    Unverifiable,
    /// `kind:"unmatched"` — a concrete span matched no rule in some group.
    Unmatched,
}

/// `hSv`'s return value (cc-238 @290146725): the span that failed and the group
/// it failed against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClampMiss {
    /// The offending span (the whole trimmed command for `Unverifiable`).
    pub span: String,
    /// The clamp group the span was checked against (the FIRST group for
    /// `Unverifiable`, matching `group: t[0] ?? []`).
    pub group: Vec<String>,
    /// Which arm produced this miss.
    pub kind: ClampMissKind,
}

/// `Jkf` (cc-238 @290146703) — a surface that cannot be expressed as a Bash
/// command form at all.
#[must_use]
pub fn clamp_surface_deny_message(tool_name: &str) -> String {
    format!(
        "Permission to use {tool_name} has been denied: this agent carries a per-spawn \
bashCommandClamp, which scopes shell execution to a fixed set of Bash command forms \u{2014} \
this surface cannot match them. Use the clamped Bash forms instead."
    )
}

/// `gCi` (cc-238 @290147095) / `Gfm` (@294395082) — the FAIL-CLOSED verdict when
/// the permission check itself crashed while a clamp is active. Both oracle
/// helpers render the same template; `Gfm` is `gCi` with `"PowerShell"` bound.
#[must_use]
pub fn clamp_crash_deny_message(tool_name: &str) -> String {
    format!(
        "The {tool_name} permission check crashed and this agent carries a per-spawn \
bashCommandClamp; denying rather than running an unverified command."
    )
}

/// `M8n`'s clamp arm (cc-238 @290160634) — the Bash denial, with the two
/// alternative tails selected by [`ClampMiss::kind`].
///
/// `command` is the RAW tool input command; the message trims it exactly as the
/// oracle does (`${e.command.trim()}`). The `unmatched` tail JSON-quotes the
/// span (`${Ie(a.span)}`) and joins the group with `", "`.
#[must_use]
pub fn clamp_bash_deny_message(tool_name: &str, command: &str, miss: &ClampMiss) -> String {
    let head = format!(
        "Permission to use {tool_name} with command {} has been denied: this agent's Bash use is \
clamped to a fixed set of command forms (per-spawn bashCommandClamp), and ",
        command.trim()
    );
    let tail = match miss.kind {
        ClampMissKind::Unverifiable => "the command has structure the clamp cannot verify \
(substitution, control flow, or an undecomposable compound) \u{2014} no clamp rule can admit it. \
Issue plain commands matching the clamped forms."
            .to_string(),
        ClampMissKind::Unmatched => format!(
            "the span {} matches none of them. Allowed forms: {}",
            serde_json::Value::String(miss.span.clone()),
            miss.group.join(", ")
        ),
    };
    head + &tail
}

/// `hSv` (cc-238 @290146725) — does `command` satisfy EVERY clamp group?
///
/// Returns `None` when it does (the call proceeds to the ordinary permission
/// walk), or the FIRST miss otherwise.
///
/// ```js
/// async function hSv(e,t){
///   if(t.length===0)return null;
///   let r={span:e.command.trim(),group:t[0]??[],kind:"unverifiable"};
///   let n=await anr(e.command); if(n===c1e)return r;              // parse abort
///   let o=n?CLt(e.command,n):{kind:"simple",commands:[],…};
///   if(o.kind==="too-complex")return r;
///   let i=DLa(e.command); if(i===null||i.length===0)return r;      // no spans
///   let s=to([...i,...o.commands.map(a=>a.text)]);
///   for(let a of t){                                              // EVERY group
///     let l=new Map;
///     for(let c of a){let u=Lp(c);
///       if(u.toolName!==$p.name||u.ruleContent===void 0||u.ruleContent==="")continue;
///       l.set(u.ruleContent,{source:"session",ruleBehavior:"allow",ruleValue:u})}
///     for(let c of s){let u={...e,command:c};
///       if(!(O8n(u,l,"exact",…).length>0||O8n(u,l,"prefix",…).length>0))
///         return{span:c,group:a,kind:"unmatched"}}}
///   return null}
/// ```
///
/// Port mapping:
/// * `DLa` (span split) → [`crate::shell_command::split_command`];
/// * `anr`/`CLt` (tree-sitter parse + flatten) → [`crate::bash_ast_security::parse_for_security`],
///   available only under the `bash-ast` feature. WITHOUT that feature the
///   too-complex arm cannot fire, so a compound the AST would have rejected is
///   still checked span-by-span — strictly more conservative than skipping the
///   clamp, and it can only produce an `Unmatched` deny, never an allow;
/// * `O8n(...,"exact"|"prefix", ruleBehavior:"allow")` →
///   [`crate::shell_command::command_exact_allowed`] /
///   [`crate::shell_command::command_fully_allowed`];
/// * `Lp` (rule-string parse) → [`crate::rule::PermissionRuleValue::from_rule_string`],
///   with the same `toolName === "Bash" && ruleContent non-empty` filter, so a
///   tool-wide `Bash` or a foreign `Read(...)` entry in a group contributes
///   NOTHING (and therefore cannot admit any span).
#[must_use]
pub fn find_clamp_miss(command: &str, clamps: &[Vec<String>]) -> Option<ClampMiss> {
    if clamps.is_empty() {
        return None;
    }
    let unverifiable = || ClampMiss {
        span: command.trim().to_string(),
        group: clamps.first().cloned().unwrap_or_default(),
        kind: ClampMissKind::Unverifiable,
    };

    let mut spans: Vec<String> = Vec::new();
    #[cfg(feature = "bash-ast")]
    {
        use crate::bash_ast_security::{parse_for_security, ParseForSecurityResult};
        match parse_for_security(command) {
            // `o.kind === "too-complex"` → unverifiable.
            ParseForSecurityResult::TooComplex { .. } => return Some(unverifiable()),
            // `n == null` — tree-sitter unavailable. The oracle does NOT bail
            // here: it substitutes `{kind:"simple",commands:[]}` and carries on
            // with the `DLa` spans alone. (The bail arm is `n === c1e`, the
            // tree-sitter-differential ABORT, which this port surfaces as
            // `TooComplex` via `pre_check_too_complex`.)
            ParseForSecurityResult::ParseUnavailable => {}
            ParseForSecurityResult::Simple { commands } => {
                // `...o.commands.map(a => a.text)` — added AFTER the `DLa` spans.
                for cmd in &commands {
                    if !cmd.text.trim().is_empty() {
                        spans.push(cmd.text.clone());
                    }
                }
            }
        }
    }
    // `DLa(e.command)` — the span split. Prepended so the `to(...)` order
    // matches the oracle (`[...i, ...o.commands.map(...)]`).
    let mut ordered: Vec<String> = split_command(command)
        .into_iter()
        .filter(|span| !span.trim().is_empty())
        .collect();
    if ordered.is_empty() {
        return Some(unverifiable());
    }
    ordered.append(&mut spans);
    // `to(...)` — unique, first-seen order.
    let mut unique: Vec<String> = Vec::new();
    for span in ordered {
        if !unique.contains(&span) {
            unique.push(span);
        }
    }

    for group in clamps {
        let contents = bash_rule_contents(group);
        let rules: Vec<&str> = contents.iter().map(String::as_str).collect();
        for span in &unique {
            let matched =
                command_exact_allowed(&rules, span) || command_fully_allowed(&rules, span);
            if !matched {
                return Some(ClampMiss {
                    span: span.clone(),
                    group: group.clone(),
                    kind: ClampMissKind::Unmatched,
                });
            }
        }
    }
    None
}

/// The `Lp`-filtered rule CONTENTS of one clamp group: only `Bash(<content>)`
/// entries with a NON-EMPTY content survive.
fn bash_rule_contents(group: &[String]) -> Vec<String> {
    group
        .iter()
        .filter_map(|entry| {
            let parsed = crate::rule::PermissionRuleValue::from_rule_string(entry);
            if parsed.tool_name != "Bash" {
                return None;
            }
            parsed.rule_content.filter(|content| !content.is_empty())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(rules: &[&str]) -> Vec<String> {
        rules.iter().map(|r| (*r).to_string()).collect()
    }

    #[test]
    fn no_clamp_never_misses() {
        assert_eq!(find_clamp_miss("rm -rf /", &[]), None);
    }

    #[test]
    fn a_command_inside_the_clamp_passes() {
        let clamps = vec![group(&["Bash(git status:*)", "Bash(ls:*)"])];
        assert_eq!(find_clamp_miss("git status --short", &clamps), None);
        assert_eq!(find_clamp_miss("ls -la", &clamps), None);
    }

    #[test]
    fn a_command_outside_the_clamp_misses_with_its_span() {
        let clamps = vec![group(&["Bash(git status:*)"])];
        let miss = find_clamp_miss("git push origin main", &clamps).expect("clamped");
        assert_eq!(miss.kind, ClampMissKind::Unmatched);
        assert_eq!(miss.span, "git push origin main");
        assert_eq!(miss.group, group(&["Bash(git status:*)"]));
    }

    #[test]
    fn every_group_must_admit_every_span() {
        // Two groups INTERSECT: `ls` is in group 1 but not group 2, so it misses.
        let clamps = vec![
            group(&["Bash(git status:*)", "Bash(ls:*)"]),
            group(&["Bash(git status:*)"]),
        ];
        assert_eq!(find_clamp_miss("git status", &clamps), None);
        let miss = find_clamp_miss("ls", &clamps).expect("second group rejects");
        assert_eq!(miss.group, group(&["Bash(git status:*)"]));
    }

    #[test]
    fn one_bad_span_in_a_compound_misses() {
        let clamps = vec![group(&["Bash(git status:*)"])];
        let miss = find_clamp_miss("git status && rm -rf /", &clamps).expect("clamped");
        assert_eq!(miss.kind, ClampMissKind::Unmatched);
        assert_eq!(miss.span, "rm -rf /");
    }

    #[test]
    fn a_tool_wide_or_foreign_group_entry_admits_nothing() {
        // `Lp` filter: `toolName !== "Bash"` or empty ruleContent → skipped.
        let clamps = vec![group(&["Bash", "Read(**)", "Bash()"])];
        let miss = find_clamp_miss("ls", &clamps).expect("nothing admits ls");
        assert_eq!(miss.kind, ClampMissKind::Unmatched);
    }

    #[test]
    fn an_empty_command_is_unverifiable() {
        let clamps = vec![group(&["Bash(ls:*)"])];
        let miss = find_clamp_miss("   ", &clamps).expect("no spans");
        assert_eq!(miss.kind, ClampMissKind::Unverifiable);
        assert_eq!(miss.group, group(&["Bash(ls:*)"]));
    }

    #[test]
    fn surface_deny_message_is_byte_locked() {
        assert_eq!(
            clamp_surface_deny_message(MONITOR_WEBSOCKET_SURFACE),
            "Permission to use Monitor websocket has been denied: this agent carries a per-spawn \
bashCommandClamp, which scopes shell execution to a fixed set of Bash command forms \u{2014} this \
surface cannot match them. Use the clamped Bash forms instead."
        );
    }

    #[test]
    fn crash_deny_message_is_byte_locked_for_bash_and_powershell() {
        assert_eq!(
            clamp_crash_deny_message("Bash"),
            "The Bash permission check crashed and this agent carries a per-spawn \
bashCommandClamp; denying rather than running an unverified command."
        );
        // `Gfm` is the same template with "PowerShell" bound.
        assert_eq!(
            clamp_crash_deny_message("PowerShell"),
            "The PowerShell permission check crashed and this agent carries a per-spawn \
bashCommandClamp; denying rather than running an unverified command."
        );
    }

    #[test]
    fn powershell_deny_message_is_byte_locked() {
        assert!(POWERSHELL_CLAMP_DENY_MESSAGE.starts_with(
            "Permission to use PowerShell has been denied: this agent carries a per-spawn "
        ));
        assert!(POWERSHELL_CLAMP_DENY_MESSAGE.ends_with(
            "PowerShell commands cannot match them. Use the clamped Bash forms instead."
        ));
        assert!(POWERSHELL_CLAMP_DENY_MESSAGE.contains('\u{2014}'));
    }

    #[test]
    fn bash_deny_message_unmatched_tail_is_byte_locked() {
        let miss = ClampMiss {
            span: "git push".to_string(),
            group: group(&["Bash(git status:*)", "Bash(ls:*)"]),
            kind: ClampMissKind::Unmatched,
        };
        assert_eq!(
            clamp_bash_deny_message("Bash", "  git push  ", &miss),
            "Permission to use Bash with command git push has been denied: this agent's Bash use \
is clamped to a fixed set of command forms (per-spawn bashCommandClamp), and the span \"git push\" \
matches none of them. Allowed forms: Bash(git status:*), Bash(ls:*)"
        );
    }

    #[test]
    fn bash_deny_message_unverifiable_tail_is_byte_locked() {
        let miss = ClampMiss {
            span: "x".to_string(),
            group: Vec::new(),
            kind: ClampMissKind::Unverifiable,
        };
        assert_eq!(
            clamp_bash_deny_message("Bash", "eval \"$(x)\"", &miss),
            "Permission to use Bash with command eval \"$(x)\" has been denied: this agent's Bash \
use is clamped to a fixed set of command forms (per-spawn bashCommandClamp), and the command has \
structure the clamp cannot verify (substitution, control flow, or an undecomposable compound) \
\u{2014} no clamp rule can admit it. Issue plain commands matching the clamped forms."
        );
    }

    #[test]
    fn decision_reasons_are_byte_locked() {
        assert_eq!(
            CLAMP_NO_MATCH_REASON,
            "bashCommandClamp: no clamp rule matches this command"
        );
        assert_eq!(
            CLAMP_FAIL_CLOSED_REASON,
            "bashCommandClamp fail-closed: permission check crashed"
        );
    }
}
