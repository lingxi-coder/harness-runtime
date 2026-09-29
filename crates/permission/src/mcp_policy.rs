//! MCP per-tool permission policy PRODUCER — 1:1 with claude-code `LXe`/`tir`
//! (offset ~154729744, `session.js`) plus the org-admin ceiling (`Kp`, offset
//! ~176360614; schema `cP`, offset ~154584475/155746241).
//!
//! ## Oracle algorithm
//!
//! `LXe(servers)` walks every **`dynamic`-scope** `http`/`sse` server's
//! `tools[]` array (each entry `{name, permission_policy?}`), builds the FQN
//! `mcp__<normalize(server)>__<normalize(tool)>` (`xc`/`Ul`, using the SAME
//! [`lingxi_core::types::normalize_name_for_mcp`] normalizer — verified byte-for-byte:
//! the oracle's local `ln()` in this chunk IS `normalizeNameForMCP`, not the
//! unrelated NFC/lowercase `ln()` used elsewhere in the bundle), and resolves a
//! duplicate FQN **strictest-wins** via the ordinal `{always_allow:0,
//! always_ask:1, always_deny:2}` (`E>(t[w]??-1)` — first entry wins ties,
//! rescinding is one-way). `tir(session, servers)` merges the three resulting
//! name lists into `session.{alwaysAllowRules,alwaysDenyRules,alwaysAskRules}.mcpServerPolicy`
//! — i.e. exactly the [`crate::PermissionRuleSource::McpServerPolicy`] bucket
//! this module targets.
//!
//! The org-admin ceiling (`tools[].org_max_permission`, `allow`|`ask`|`blocked`)
//! is a SEPARATE oracle mechanism: `Kp(tools)` folds it into the on-disk
//! `toolPermissions` map (only entries `!== "allow"`), which tool discovery
//! attaches as `mcpInfo.effectiveMaxPermission` and the invocation gate
//! consults directly (`e.mcpInfo?.effectiveMaxPermission==="ask"` forces a
//! prompt REGARDLESS of mode — "Drives the auto-mode `isOrgAskCeiling` gate so
//! an admin 'ask' cap forces a user prompt even in auto mode"; `"blocked"`
//! filters the tool out of the list entirely, `hI`).
//!
//! `permission/` does not and must not depend on `mcp` (no live registry
//! lookup is possible here), so this port rides the ceiling on the SAME
//! [`crate::PermissionRuleSource::McpServerPolicy`] bucket [`LXe`] builds: a
//! `blocked` ceiling becomes a tool-wide DENY rule; an `ask` ceiling becomes a
//! tool-wide ASK rule. This is an approximation of the OUTCOME, not the
//! mechanism, and it is NOT equivalent to `hI` in two observable ways the
//! eventual wirer must handle:
//!
//! * `blocked` — `hI` is a hard FILTER over the advertised tool array
//!   (`return n.mcpInfo?.effectiveMaxPermission!=="blocked"`, @158550277), so
//!   the model never sees the tool at all. A deny RULE leaves the tool in the
//!   model's tool list: the model calls it and burns a turn on the denial.
//!   Suppressing the tool needs a `mcp`-side filter at the tool-list build
//!   (`build_registered_mcp_tools`), which this crate cannot reach. `/mcp`'s
//!   tool browser additionally sorts blocked tools last (`Ah`/`Oh`
//!   @184247711) — also unmodelled.
//! * `ask` — the oracle's ceiling ask ALSO hides the always-allow row via
//!   `isAskCappedByOrg: e.tool.mcpInfo?.effectiveMaxPermission==="ask"`
//!   feeding `zn()`/`xet()` (@172369681, @177393036). A plain
//!   `McpServerPolicy` ask rule does not reproduce that; the port's
//!   equivalent channel is
//!   `PermissionCheckContext::requires_user_interaction`, which nothing sets
//!   from a ceiling.
//!
//! What the ask/deny bucket DOES reproduce faithfully is precedence:
//! [`PermissionPolicy`]'s
//! `authorize_inner` checks the WHOLE deny bucket, then the TOOL-WIDE ask
//! bucket, before ANY allow rule is consulted (`policy.rs` steps 1a-1c), so a
//! tool-wide ask/deny rule from this bucket cannot be beaten by a
//! higher-priority ALLOW rule (e.g. a session "Always Allow") any more than the
//! oracle's unconditional `effectiveMaxPermission` OR-check can — verified by
//! [`ceiling_ask_beats_a_higher_priority_allow_rule`] below.
//!
//! ## Wiring status — PRODUCER ONLY
//!
//! [`PermissionRuleSource::McpServerPolicy`] is wired end to end already
//! (`priority`, `lingxi_settings_source`, `SOURCES_BY_PRIORITY`,
//! `filesystem.rs` root resolution, `shadow.rs` display, the TUI editor label)
//! — this module is the missing PRODUCER: [`mcp_server_policy_rules`] turns an
//! already-resolved view of the dynamic MCP server set into the
//! `Vec<PermissionRule>` those call sites expect.
//!
//! It deliberately does NOT parse `McpJsonEntry`/`McpServerConfig` — neither
//! carries a `tools`/`toolPermissions` field yet (`mcp/src/json_config.rs`,
//! `mcp/src/connection.rs`), and both signal families come off the claude.ai
//! connector surface (`dynamic` scope, the `[claudeai-mcp]` diagnostics
//! prefix), not off `.mcp.json`. NOTHING IN THE TREE CALLS THIS TODAY: §1/§2
//! has no runtime effect until a composition root wires it.
//!
//! 🚨 **The wirer MUST reproduce `tir`'s call-site gate, which is not visible
//! from `tir` itself.** `tir` has exactly ONE production call site and it is
//! conditional:
//!
//! ```js
//! if(a.CLAUDE_CODE_REMOTE&&I("tengu_mcp_startup_policy_seed",!0))mo=tir(mo,Cl)
//! ```
//!
//! (@166797318). An ordinary interactive desktop session NEVER seeds these
//! rules: without `CLAUDE_CODE_REMOTE` in the environment, or with
//! `tengu_mcp_startup_policy_seed` off, the whole `mcpServerPolicy` bucket
//! stays empty. Calling [`mcp_server_policy_rules`] unconditionally would
//! start enforcing org allow/ask/deny rules in sessions where the oracle
//! never does. (The gate is deliberately NOT applied inside this producer:
//! it is an environment/feature-flag read that belongs at the composition
//! root, next to the other startup gates.)
//!
//! With that gate satisfied, the composition root that assembles the live
//! [`mcp`] registry and the [`crate::PolicyPermissionGate`] together is the
//! intended caller once those fields land: it would build one
//! [`McpServerPolicyView`] per dynamic-scope `http`/`sse` server, call
//! [`mcp_server_policy_rules`], and feed the result into
//! [`PermissionPolicy::from_rules`] (or splice it into the live rule state the
//! way `tir`'s live-recompute call site does on every MCP config change).
//! Note also `Aon`'s bypass exemption for this exact bucket
//! (`C.source==="mcpServerPolicy"&&a.CLAUDE_CODE_REMOTE&&…==="bypassPermissions"
//! &&I("tengu_mcp_server_policy_bypass_exempt",!0)`, @160209494), which the
//! port's gate does not model either. [`upstream_name_drift_warning`] is the matching diagnostic
//! producer (`[claudeai-mcp] <server>: toolPermissions has N entries but none
//! matched upstream tool names — backend name drift?`, offset ~182317179) —
//! also unwired for the same reason: it needs the LIVE discovered tool-name
//! list, which only the registry has.

use crate::result::{PermissionDecisionReason, PermissionPrompt, PermissionResult};
use crate::rule::{PermissionBehavior, PermissionRule, PermissionRuleSource, PermissionRuleValue};
use std::collections::BTreeMap;
use std::collections::HashMap;

/// `tools[].permission_policy` (oracle `cP`/`p()` schema). Declared per-tool on
/// a `dynamic`-scope `http`/`sse` server's `tools[]` array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpToolPermissionPolicy {
    /// Tool-wide allow, no prompt.
    AlwaysAllow,
    /// Force a prompt on every call.
    AlwaysAsk,
    /// Tool-wide deny.
    AlwaysDeny,
}

impl McpToolPermissionPolicy {
    /// Return the stricter declaration (`always_deny` > `always_ask` >
    /// `always_allow`) when several config records normalize to one FQN.
    #[must_use]
    pub const fn strictest(self, other: Self) -> Self {
        match (self, other) {
            (Self::AlwaysDeny, _) | (_, Self::AlwaysDeny) => Self::AlwaysDeny,
            (Self::AlwaysAsk, _) | (_, Self::AlwaysAsk) => Self::AlwaysAsk,
            (Self::AlwaysAllow, Self::AlwaysAllow) => Self::AlwaysAllow,
        }
    }
}

/// `tools[].org_max_permission` / on-disk `toolPermissions` value (oracle
/// `LTt`: `["allow","ask","blocked"]`). "Org admin's per-tool ceiling."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpToolMaxPermission {
    /// No ceiling — the absence of a restriction (`Kp` never emits an entry
    /// for this case).
    Allow,
    /// Force a prompt regardless of mode (`isOrgAskCeiling`).
    Ask,
    /// Filter the tool out entirely (`hI`'s hard filter).
    Blocked,
}

/// One `tools[]` entry for a single server (oracle `cP`).
#[derive(Debug, Clone)]
pub struct McpServerToolDecl {
    /// Upstream (unprefixed) tool name.
    pub name: String,
    /// §1 — this tool's declared `permission_policy`, if any.
    pub permission_policy: Option<McpToolPermissionPolicy>,
    /// §2 — this tool's declared `org_max_permission`, if any. NOTE: the
    /// producer reads the ceiling through
    /// [`McpServerPolicyView::tool_permissions`], not this field — the
    /// composition root is expected to fold `org_max_permission` into that map
    /// first (mirroring oracle `Kp`) before calling
    /// [`mcp_server_policy_rules`]. Kept here so a caller can carry the raw
    /// per-tool declaration through unchanged.
    pub org_max_permission: Option<McpToolMaxPermission>,
}

/// The already-resolved view of one MCP server this module needs — supplied by
/// the composition root, NOT parsed here (see module docs on the file-ownership
/// boundary).
#[derive(Debug, Clone, Default)]
pub struct McpServerPolicyView {
    /// The server's configured name (unnormalized — normalized inside this
    /// module when building each FQN).
    pub server_name: String,
    /// `config.scope === "dynamic"` (oracle `tir`'s filter). `permission_policy`
    /// is walked ONLY for dynamic-scope servers; the ceiling
    /// (`org_max_permission` / `toolPermissions`) is scope-independent (the
    /// oracle's tool-discovery attach point never checks scope).
    pub is_dynamic_scope: bool,
    /// Whether this server's transport is `http` or `sse` — `LXe` skips every
    /// other transport (`u.type!=="http"&&u.type!=="sse"`); the ceiling
    /// additionally covers `claudeai-proxy`, which this port does not model.
    pub is_http_or_sse: bool,
    /// This server's declared `tools[]` array (§1).
    pub tools: Vec<McpServerToolDecl>,
    /// On-disk `toolPermissions` map (sse/http), keyed by upstream tool name —
    /// independent of `tools[]`.
    pub tool_permissions: HashMap<String, McpToolMaxPermission>,
}

/// Ordinal severity shared by both signal families — `always_allow`/`allow` are
/// the baseline (0), `always_ask`/`ask` force a prompt (1), `always_deny`/
/// `blocked` are absolute (2). Strictest (highest) wins across ALL signals for
/// a given FQN, mirroring `LXe`'s `E>(t[w]??-1)` comparison.
fn policy_severity(p: McpToolPermissionPolicy) -> u8 {
    match p {
        McpToolPermissionPolicy::AlwaysAllow => 0,
        McpToolPermissionPolicy::AlwaysAsk => 1,
        McpToolPermissionPolicy::AlwaysDeny => 2,
    }
}

fn ceiling_severity(c: McpToolMaxPermission) -> Option<u8> {
    match c {
        // Kp: `if(r.org_max_permission&&r.org_max_permission!=="allow")…` — an
        // "allow" ceiling contributes NOTHING (no entry), it is the absence of
        // a restriction, not a signal of its own.
        McpToolMaxPermission::Allow => None,
        McpToolMaxPermission::Ask => Some(1),
        McpToolMaxPermission::Blocked => Some(2),
    }
}

/// NOTE: `Blocked` maps to [`PermissionBehavior::Deny`], which is NOT what
/// `hI` does — see the module docs' `blocked` bullet. The tool stays visible
/// to the model; only the call is refused.
fn behavior_for_severity(severity: u8) -> PermissionBehavior {
    match severity {
        0 => PermissionBehavior::Allow,
        1 => PermissionBehavior::Ask,
        _ => PermissionBehavior::Deny,
    }
}

/// Build the `mcp__<server>__<tool>` FQN — 1:1 with oracle `xc`/`Ul`, which
/// normalize BOTH segments through `normalizeNameForMCP` (this chunk's local
/// `ln()` is that normalizer, not the unrelated NFC-form `ln()` used
/// elsewhere in the bundle).
fn fqn(server_name: &str, tool_name: &str) -> String {
    format!(
        "mcp__{}__{}",
        lingxi_core::types::normalize_name_for_mcp(server_name),
        lingxi_core::types::normalize_name_for_mcp(tool_name)
    )
}

/// The PRODUCER: turn a resolved view of the MCP server set into the
/// [`PermissionRuleSource::McpServerPolicy`] rules `LXe`+`tir` would have
/// produced, extended with the org-admin ceiling folded into the SAME bucket
/// (see module docs). Every returned rule is tool-wide (`rule_content: None`)
/// and tagged [`PermissionRuleSource::McpServerPolicy`].
///
/// Per-FQN resolution is STRICTEST-WINS across every signal that names that
/// FQN — `permission_policy` (dynamic-scope `http`/`sse` only) and the ceiling
/// (`org_max_permission` funneled through `tool_permissions`, any scope) alike
/// — matching `LXe`'s ordinal comparison, generalized to the union of both
/// signal families instead of `permission_policy` alone.
#[must_use]
pub fn mcp_server_policy_rules(servers: &[McpServerPolicyView]) -> Vec<PermissionRule> {
    let mut severity: HashMap<String, u8> = HashMap::new();

    for server in servers {
        if !server.is_http_or_sse {
            continue;
        }
        // §1 — `permission_policy`, dynamic scope only (`tir`'s
        // `scope==="dynamic"` filter).
        if server.is_dynamic_scope {
            for tool in &server.tools {
                let Some(policy) = tool.permission_policy else {
                    continue;
                };
                let candidate = policy_severity(policy);
                let name = fqn(&server.server_name, &tool.name);
                let entry = severity.entry(name).or_insert(candidate);
                if candidate > *entry {
                    *entry = candidate;
                }
            }
        }
        // §2 — the org-admin ceiling. Scope-independent (oracle tool discovery
        // never gates `toolPermissions`/`effectiveMaxPermission` on scope).
        // `org_max_permission` folds into the SAME on-disk `toolPermissions`
        // channel (`Kp`), so both are read through `tool_permissions` here;
        // the composition root is expected to have already merged
        // `tools[].org_max_permission` into it when building the view.
        for (tool_name, ceiling) in &server.tool_permissions {
            let Some(candidate) = ceiling_severity(*ceiling) else {
                continue;
            };
            let name = fqn(&server.server_name, tool_name);
            let entry = severity.entry(name).or_insert(candidate);
            if candidate > *entry {
                *entry = candidate;
            }
        }
    }

    let mut names: Vec<&String> = severity.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| PermissionRule {
            value: PermissionRuleValue {
                tool_name: name.clone(),
                rule_content: None,
            },
            behavior: behavior_for_severity(severity[name]),
            source: PermissionRuleSource::McpServerPolicy,
        })
        .collect()
}

/// The matching diagnostic (oracle offset ~182317179): `toolPermissions` was
/// declared with entries, but none of them named a tool the server actually
/// advertised — almost always a backend rename the local config missed.
/// Byte-locked to `` `[claudeai-mcp] ${server}: toolPermissions has ${N}
/// entries but none matched upstream tool names — backend name drift?` ``.
///
/// Unwired (see module docs): needs the LIVE discovered tool-name list, which
/// only the registry has.
#[must_use]
pub fn upstream_name_drift_warning(
    server_name: &str,
    tool_permissions: &HashMap<String, McpToolMaxPermission>,
    discovered_tool_names: &[String],
) -> Option<String> {
    if tool_permissions.is_empty() {
        return None;
    }
    let any_matched = discovered_tool_names
        .iter()
        .any(|discovered| tool_permissions.contains_key(discovered));
    if any_matched {
        return None;
    }
    Some(format!(
        "[claudeai-mcp] {server_name}: toolPermissions has {} entries but none matched upstream tool names \u{2014} backend name drift?",
        tool_permissions.len()
    ))
}

/// Collapse config-side per-tool policy declarations into the existing
/// `McpServerPolicy` rule bucket. This is the DTO-neutral equivalent of the
/// host producer: callers can pass the already-resolved server tool records,
/// while the registry/discovery cache remains untouched.
pub trait McpPolicyTool {
    /// Return the raw upstream tool name.
    fn policy_tool_name(&self) -> &str;
    /// Return this tool's optional declared permission policy.
    fn policy_permission(&self) -> Option<McpToolPermissionPolicy>;
}

impl McpPolicyTool for McpServerToolDecl {
    fn policy_tool_name(&self) -> &str {
        &self.name
    }

    fn policy_permission(&self) -> Option<McpToolPermissionPolicy> {
        self.permission_policy
    }
}

impl McpPolicyTool for lingxi_core::host::McpConfiguredToolPolicyDto {
    fn policy_tool_name(&self) -> &str {
        &self.name
    }

    fn policy_permission(&self) -> Option<McpToolPermissionPolicy> {
        self.permission_policy.map(|policy| match policy {
            lingxi_core::host::McpToolPermissionPolicy::AlwaysAllow => {
                McpToolPermissionPolicy::AlwaysAllow
            }
            lingxi_core::host::McpToolPermissionPolicy::AlwaysAsk => {
                McpToolPermissionPolicy::AlwaysAsk
            }
            lingxi_core::host::McpToolPermissionPolicy::AlwaysDeny => {
                McpToolPermissionPolicy::AlwaysDeny
            }
        })
    }
}

#[must_use]
pub fn permission_rules_from_mcp_tool_policies<T: McpPolicyTool>(
    server_name: &str,
    tools: &[T],
) -> Vec<PermissionRule> {
    let normalized_server = lingxi_core::types::normalize_name_for_mcp(server_name);
    let mut collapsed = BTreeMap::<String, McpToolPermissionPolicy>::new();
    for tool in tools {
        let Some(policy) = tool.policy_permission() else {
            continue;
        };
        let tool_name = format!(
            "mcp__{normalized_server}__{}",
            lingxi_core::types::normalize_name_for_mcp(tool.policy_tool_name())
        );
        collapsed
            .entry(tool_name)
            .and_modify(|current| *current = current.strictest(policy))
            .or_insert(policy);
    }
    collapsed
        .into_iter()
        .map(|(tool_name, policy)| PermissionRule {
            value: PermissionRuleValue {
                tool_name,
                rule_content: None,
            },
            behavior: behavior_from_policy(policy),
            source: PermissionRuleSource::McpServerPolicy,
        })
        .collect()
}

fn behavior_from_policy(policy: McpToolPermissionPolicy) -> PermissionBehavior {
    match policy {
        McpToolPermissionPolicy::AlwaysAllow => PermissionBehavior::Allow,
        McpToolPermissionPolicy::AlwaysAsk => PermissionBehavior::Ask,
        McpToolPermissionPolicy::AlwaysDeny => PermissionBehavior::Deny,
    }
}

fn max_permission_severity(ceiling: McpToolMaxPermission) -> u8 {
    match ceiling {
        McpToolMaxPermission::Allow => 0,
        McpToolMaxPermission::Ask => 1,
        McpToolMaxPermission::Blocked => 2,
    }
}

fn clamp_behavior(
    base: PermissionBehavior,
    local_ceiling: Option<McpToolMaxPermission>,
    server_ceiling: Option<McpToolMaxPermission>,
    requires_user_interaction: bool,
    app_capability_authorized: bool,
) -> PermissionBehavior {
    let mut ceiling = local_ceiling.or(server_ceiling);
    if let (Some(local), Some(server)) = (local_ceiling, server_ceiling) {
        ceiling = Some(
            if max_permission_severity(local) >= max_permission_severity(server) {
                local
            } else {
                server
            },
        );
    }
    let mut out = match (base, ceiling) {
        (PermissionBehavior::Deny, _) => PermissionBehavior::Deny,
        (_, Some(McpToolMaxPermission::Blocked)) => PermissionBehavior::Deny,
        (PermissionBehavior::Ask, _) => PermissionBehavior::Ask,
        (_, Some(McpToolMaxPermission::Ask)) => PermissionBehavior::Ask,
        (PermissionBehavior::Allow, _) => PermissionBehavior::Allow,
    };
    if out == PermissionBehavior::Allow && requires_user_interaction {
        out = PermissionBehavior::Ask;
    }
    if out != PermissionBehavior::Deny && !app_capability_authorized {
        out = PermissionBehavior::Deny;
    }
    out
}

/// Apply local/server MCP ceilings and tool-owned interaction requirements to
/// an already-resolved permission result. Every transform is tighten-only:
/// it never turns an ask/deny into an allow, and duplicate ceilings resolve to
/// the strictest (`blocked` > `ask` > `allow`).
#[must_use]
pub fn clamp_mcp_permission_result(
    result: PermissionResult,
    tool_name: &str,
    local_ceiling: Option<McpToolMaxPermission>,
    server_ceiling: Option<McpToolMaxPermission>,
    requires_user_interaction: bool,
    app_capability_authorized: bool,
) -> PermissionResult {
    let metadata = match &result {
        PermissionResult::Allow { metadata, .. }
        | PermissionResult::Ask { metadata, .. }
        | PermissionResult::Deny { metadata, .. } => metadata.clone(),
    };
    let base = match result {
        PermissionResult::Allow { .. } => PermissionBehavior::Allow,
        PermissionResult::Ask { .. } => PermissionBehavior::Ask,
        PermissionResult::Deny { .. } => PermissionBehavior::Deny,
    };
    let final_behavior = clamp_behavior(
        base,
        local_ceiling,
        server_ceiling,
        requires_user_interaction,
        app_capability_authorized,
    );
    if final_behavior == base {
        return result;
    }
    let interaction_ask = final_behavior == PermissionBehavior::Ask
        && base == PermissionBehavior::Allow
        && requires_user_interaction;
    match final_behavior {
        PermissionBehavior::Allow => result,
        PermissionBehavior::Ask => PermissionResult::Ask {
            reason: if interaction_ask {
                PermissionDecisionReason::PermissionPromptTool {
                    tool_name: tool_name.to_string(),
                }
            } else {
                PermissionDecisionReason::Other {
                    reason: format!("MCP tool {tool_name} requires approval"),
                }
            },
            prompt: PermissionPrompt {
                title: "Permission required".into(),
                message: format!("MCP tool {tool_name} requires approval"),
                options: vec!["Allow once".into(), "Deny".into()],
            },
            pending_classifier_check: None,
            metadata,
        },
        PermissionBehavior::Deny => PermissionResult::Deny {
            reason: PermissionDecisionReason::Other {
                reason: format!("MCP tool {tool_name} is blocked"),
            },
            explanation: Some(format!("MCP tool {tool_name} is blocked")),
            metadata,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::PermissionMode;
    use crate::policy::PermissionPolicy;
    use crate::result::PermissionResult;

    fn view(
        server_name: &str,
        dynamic: bool,
        tools: Vec<(&str, Option<McpToolPermissionPolicy>)>,
    ) -> McpServerPolicyView {
        McpServerPolicyView {
            server_name: server_name.to_string(),
            is_dynamic_scope: dynamic,
            is_http_or_sse: true,
            tools: tools
                .into_iter()
                .map(|(name, permission_policy)| McpServerToolDecl {
                    name: name.to_string(),
                    permission_policy,
                    org_max_permission: None,
                })
                .collect(),
            tool_permissions: HashMap::new(),
        }
    }

    #[test]
    fn always_allow_produces_an_allow_rule() {
        let rules = mcp_server_policy_rules(&[view(
            "srv",
            true,
            vec![("tool", Some(McpToolPermissionPolicy::AlwaysAllow))],
        )]);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].value.tool_name, "mcp__srv__tool");
        assert_eq!(rules[0].behavior, PermissionBehavior::Allow);
        assert_eq!(rules[0].source, PermissionRuleSource::McpServerPolicy);
    }

    #[test]
    fn always_deny_produces_a_deny_rule() {
        let rules = mcp_server_policy_rules(&[view(
            "srv",
            true,
            vec![("tool", Some(McpToolPermissionPolicy::AlwaysDeny))],
        )]);
        assert_eq!(rules[0].behavior, PermissionBehavior::Deny);
    }

    #[test]
    fn non_dynamic_scope_is_never_walked() {
        // §1 is `tir`'s `scope==="dynamic"` filter — a project/user-scoped
        // server's `tools[].permission_policy` must not produce a rule.
        let rules = mcp_server_policy_rules(&[view(
            "srv",
            false,
            vec![("tool", Some(McpToolPermissionPolicy::AlwaysDeny))],
        )]);
        assert!(rules.is_empty());
    }

    #[test]
    fn non_http_sse_transport_is_skipped() {
        let mut v = view(
            "srv",
            true,
            vec![("tool", Some(McpToolPermissionPolicy::AlwaysDeny))],
        );
        v.is_http_or_sse = false;
        assert!(mcp_server_policy_rules(&[v]).is_empty());
    }

    #[test]
    fn duplicate_fqn_is_strictest_wins_regardless_of_order() {
        // `LXe`'s `E>(t[w]??-1)`: a later `always_allow` after an earlier
        // `always_deny` for the SAME normalized name must not downgrade it.
        // `normalize_name_for_mcp` is case-PRESERVING (it substitutes invalid
        // chars, it does not lowercase — verified against the oracle's local
        // `ln()` in this chunk), so the collision must come from two DIFFERENT
        // raw names that substitute to the same normalized string: `.` and `_`
        // both survive as `_`.
        let rules = mcp_server_policy_rules(&[view(
            "srv",
            true,
            vec![
                ("my.tool", Some(McpToolPermissionPolicy::AlwaysDeny)),
                ("my_tool", Some(McpToolPermissionPolicy::AlwaysAllow)),
            ],
        )]);
        assert_eq!(rules.len(), 1, "both entries normalize to the same FQN");
        assert_eq!(rules[0].value.tool_name, "mcp__srv__my_tool");
        assert_eq!(rules[0].behavior, PermissionBehavior::Deny);
    }

    #[test]
    fn trait_config_dto_uses_the_same_strictest_producer() {
        let rules = permission_rules_from_mcp_tool_policies(
            "srv",
            &[
                lingxi_core::host::McpConfiguredToolPolicyDto {
                    name: "write".into(),
                    permission_policy: Some(
                        lingxi_core::host::McpToolPermissionPolicy::AlwaysAllow,
                    ),
                    org_max_permission: None,
                },
                lingxi_core::host::McpConfiguredToolPolicyDto {
                    name: "write".into(),
                    permission_policy: Some(lingxi_core::host::McpToolPermissionPolicy::AlwaysDeny),
                    org_max_permission: None,
                },
            ],
        );
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].behavior, PermissionBehavior::Deny);
    }

    #[test]
    fn strictest_wins_the_other_order_too() {
        let rules = mcp_server_policy_rules(&[view(
            "srv",
            true,
            vec![
                ("tool", Some(McpToolPermissionPolicy::AlwaysAsk)),
                ("tool", Some(McpToolPermissionPolicy::AlwaysDeny)),
            ],
        )]);
        assert_eq!(rules[0].behavior, PermissionBehavior::Deny);
    }

    #[test]
    fn tool_name_and_server_name_are_normalized_into_the_fqn() {
        let rules = mcp_server_policy_rules(&[view(
            "My Server",
            true,
            vec![("My Tool", Some(McpToolPermissionPolicy::AlwaysDeny))],
        )]);
        assert_eq!(rules[0].value.tool_name, "mcp__My_Server__My_Tool");
    }

    #[test]
    fn ceiling_blocked_produces_a_deny_rule_regardless_of_scope() {
        let mut v = view("srv", false, vec![]);
        v.tool_permissions
            .insert("tool".to_string(), McpToolMaxPermission::Blocked);
        let rules = mcp_server_policy_rules(&[v]);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].value.tool_name, "mcp__srv__tool");
        assert_eq!(rules[0].behavior, PermissionBehavior::Deny);
    }

    #[test]
    fn ceiling_ask_produces_an_ask_rule() {
        let mut v = view("srv", false, vec![]);
        v.tool_permissions
            .insert("tool".to_string(), McpToolMaxPermission::Ask);
        let rules = mcp_server_policy_rules(&[v]);
        assert_eq!(rules[0].behavior, PermissionBehavior::Ask);
    }

    #[test]
    fn ceiling_allow_contributes_no_rule() {
        // Kp: `org_max_permission!=="allow"` — "allow" is the absence of a
        // ceiling, never an entry.
        let mut v = view("srv", false, vec![]);
        v.tool_permissions
            .insert("tool".to_string(), McpToolMaxPermission::Allow);
        assert!(mcp_server_policy_rules(&[v]).is_empty());
    }

    #[test]
    fn ceiling_ask_beats_a_local_always_allow_on_the_same_tool() {
        // The ceiling and `permission_policy` are independent signals in the
        // oracle; this port folds both into ONE strictest-wins bucket, so an
        // org "ask" ceiling must survive even when the same server's
        // `tools[]` separately declares `always_allow` for that tool.
        let mut v = view(
            "srv",
            true,
            vec![("tool", Some(McpToolPermissionPolicy::AlwaysAllow))],
        );
        v.tool_permissions
            .insert("tool".to_string(), McpToolMaxPermission::Ask);
        let rules = mcp_server_policy_rules(&[v]);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].behavior, PermissionBehavior::Ask);
    }

    /// Confirms the "ride the bucket" design actually reproduces
    /// `isOrgAskCeiling` ("forces a user prompt even in auto mode"): a
    /// tool-wide ASK rule from this producer must win over a HIGHER-PRIORITY
    /// allow rule (e.g. a session "Always Allow"), because
    /// [`PermissionPolicy::authorize`]'s deny/tool-wide-ask walk (steps 1a-1c)
    /// runs entirely BEFORE any allow rule is consulted — independent of
    /// [`PermissionRuleSource::priority`], which only breaks ties WITHIN a
    /// bucket.
    #[test]
    fn ceiling_ask_beats_a_higher_priority_allow_rule() {
        let mut v = view("srv", false, vec![]);
        v.tool_permissions
            .insert("tool".to_string(), McpToolMaxPermission::Ask);
        let mut rules = mcp_server_policy_rules(&[v]);
        // Session is a HIGHER-priority source than McpServerPolicy
        // (`PermissionRuleSource::priority`), yet must not win.
        rules.push(PermissionRule::allow_tool_session("mcp__srv__tool"));
        let policy = PermissionPolicy::from_rules(PermissionMode::Auto, rules);
        let result = policy.authorize("mcp__srv__tool", &serde_json::json!({}));
        assert!(
            matches!(result, PermissionResult::Ask { .. }),
            "expected the org-ceiling ask rule to win over the session allow rule, got {result:?}"
        );
    }

    #[test]
    fn drift_warning_fires_only_when_nothing_matched() {
        let mut perms = HashMap::new();
        perms.insert("renamed_tool".to_string(), McpToolMaxPermission::Ask);
        let warning = upstream_name_drift_warning("srv", &perms, &["current_tool".to_string()]);
        assert_eq!(
            warning.as_deref(),
            Some(
                "[claudeai-mcp] srv: toolPermissions has 1 entries but none matched upstream tool names \u{2014} backend name drift?"
            )
        );
    }

    #[test]
    fn drift_warning_silent_on_any_match() {
        let mut perms = HashMap::new();
        perms.insert("tool".to_string(), McpToolMaxPermission::Ask);
        assert_eq!(
            upstream_name_drift_warning("srv", &perms, &["tool".to_string()]),
            None
        );
    }

    #[test]
    fn drift_warning_silent_when_empty() {
        assert_eq!(
            upstream_name_drift_warning("srv", &HashMap::new(), &[]),
            None
        );
    }

    #[test]
    fn clamp_ceiling_and_interaction_only_tighten_an_allow() {
        let allow = PermissionResult::Allow {
            reason: PermissionDecisionReason::PermissionMode {
                mode: PermissionMode::Default,
            },
            updated_input: None,
            update_destination: None,
            metadata: crate::result::PermissionMetadata::default(),
        };
        let ask = clamp_mcp_permission_result(
            allow.clone(),
            "mcp__srv__tool",
            Some(McpToolMaxPermission::Ask),
            Some(McpToolMaxPermission::Allow),
            false,
            true,
        );
        assert!(matches!(ask, PermissionResult::Ask { .. }));

        let interaction = clamp_mcp_permission_result(
            allow.clone(),
            "mcp__srv__interactive",
            None,
            None,
            true,
            true,
        );
        assert!(matches!(
            interaction,
            PermissionResult::Ask {
                reason: PermissionDecisionReason::PermissionPromptTool { ref tool_name },
                ..
            } if tool_name == "mcp__srv__interactive"
        ));

        let denied = clamp_mcp_permission_result(
            allow,
            "mcp__srv__blocked",
            Some(McpToolMaxPermission::Ask),
            Some(McpToolMaxPermission::Blocked),
            false,
            true,
        );
        assert!(matches!(denied, PermissionResult::Deny { .. }));
    }
}
