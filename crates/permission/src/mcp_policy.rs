//! MCP permission-rule producers and tighten-only tool permission checks.
//!
//! There are two distinct policy channels in the reference implementation:
//!
//! * Dynamic `http`/`sse` server `tools[].permission_policy` declarations
//!   (`always_allow`, `always_ask`, `always_deny`) can produce the
//!   `mcpServerPolicy` rule bucket. [`mcp_server_policy_rules`] retains that
//!   producer: normalized duplicate tool names resolve to the strictest
//!   policy, and source citation remains [`crate::PermissionRuleSource::McpServerPolicy`].
//! * Organization `tools[].org_max_permission` / `toolPermissions` ceilings
//!   (`allow`, `ask`, `blocked`) are tool metadata. They cannot be treated as
//!   ordinary user permission rules: `ask` must force approval and suppress
//!   persistent grants, while `blocked` removes a tool from the model catalog.
//!
//! ## Current runtime wiring
//!
//! MCP configuration now carries both `tools` and `toolPermissions` in
//! `mcp::McpServerConfig`. `McpClient::apply_config_permission_ceilings`
//! resolves declarations against discovered raw tool names and reports
//! upstream name drift. The shared `tool_mcp::build_registered_mcp_tools`
//! builder and the Desktop agent MCP builder carry organization-only ceilings
//! onto each tool. Organization-blocked tools are filtered before model exposure.
//! A `permission_policy` declaration is not an organization ceiling.
//!
//! `MCPTool::check_permissions` calls [`clamp_mcp_permission_result`] for
//! explicitly supplied ceilings and `requiresUserInteraction`. Its separate organization
//! metadata feeds `tool_check_permission_ceiling`, which the orchestrator
//! supplies to [`crate::PolicyPermissionGate`]. The dispatch layer preserves
//! protected MCP asks over allow rules, modes, and permission-handler rescue;
//! organization asks also suppress the always-allow option. These paths do
//! not require `permission` to depend on the live MCP registry.
//!
//! Desktop and Mobile compose startup rules through
//! [`permission_rules_from_mcp_tool_policies`] after the MCP-owned
//! `startup_permission_policies` scope/host gate. Native 2.1.318 seeds dynamic
//! HTTP/SSE policy rules only under `CLAUDE_CODE_REMOTE` (branded as
//! `LINGXI_REMOTE`) and `tengu_mcp_startup_policy_seed` (@278315381).
//! SDK channel policies contribute only ask/deny rules, never always-allow.
//! Ordinary local server declarations do not become rules or metadata ceilings.
//! SDK initialize also accepts per-tool policies after boot. The registry
//! accumulates its ask/deny declarations without re-registering a live SDK
//! transport. SDK registration and every shared tool-catalog rebuild replace
//! the live `McpServerPolicy` bucket atomically from a complete active-server
//! snapshot; retirement removes only the retired server's rules. Other
//! permission sources and organization ceilings remain independent.
//! Remote bypass mode may exempt this bucket's ask rules under
//! `tengu_mcp_server_policy_bypass_exempt` (@270627134); deny rules and org
//! ceilings remain enforced. This startup wiring does not establish parity
//! with the full external cloud connector identity and refresh lifecycle.
//!
//! Native 2.1.318's organization filter is `Qj` (@266988106); its tool-check
//! organization-ask branches are @270622455/@270628745. These are distinct
//! from the older rule-producer offsets used by the pure helpers below.

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
                original_rule: None,
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
/// Pure formatter; the live equivalent is emitted by
/// `McpClient::apply_config_permission_ceilings` after discovery resolves raw
/// tool names. This helper does not query the registry.
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
                original_rule: None,
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
                    reason: format!("MCP tool {tool_name} requires approval").into(),
                }
            },
            prompt: PermissionPrompt {
                title: "Permission required".into(),
                message: format!("MCP tool {tool_name} requires approval").into(),
                options: vec!["Allow once".into(), "Deny".into()],
            },
            pending_classifier_check: None,
            metadata,
        },
        PermissionBehavior::Deny => PermissionResult::Deny {
            reason: PermissionDecisionReason::Other {
                reason: format!("MCP tool {tool_name} is blocked").into(),
            },
            explanation: Some(format!("MCP tool {tool_name} is blocked").into()),
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
        let input = lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}")
            .expect("empty test input is valid JSON");
        let result = policy.authorize("mcp__srv__tool", &input);
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
