//! PER-CALL permission LAYERS — the port of claude-code `gn(toolUseContext)`
//! (2.1.238 binary @287028951) and its two companions
//! `applyContextLayers` (`V3r`) / `stripWideningPermissionLayers` (`qLa`).
//!
//! ## Why this module exists
//!
//! Upstream never stores "the" permission context. It stores a BASE
//! `toolPermissionContext` on the app state plus an ordered
//! `toolUseContext.permissionLayers` array, and FOLDS the two together on every
//! single permission check:
//!
//! ```js
//! function gn(e){
//!   let t=e.getAppState().toolPermissionContext;
//!   let n=e.permissionLayers; if(!n)return t;
//!   let o=n.findLast(i=>i.kind==="working_directory");
//!   for(let i of n) switch(i.kind){
//!     case"allowed_tools":     t=ULa(t,[...i.allowedTools]); break;
//!     case"disallowed_tools":  t=jLa(t,[...i.disallowedTools]); break;
//!     case"bash_command_clamp":t={...t,bashCommandClamps:[...t.bashCommandClamps??[],i.rules]};break;
//!     case"avoid_prompts":     if(!t.shouldAvoidPermissionPrompts)t={...t,shouldAvoidPermissionPrompts:!0};break;
//!     case"permission_mode":   {if(i.mode==="bypassPermissions"&&(gq()||!t.isBypassPermissionsModeAvailable))break;
//!                               t={...t,mode:i.mode};break}
//!     case"working_directory": if(i===o&&!t.additionalWorkingDirectories.has(i.directory)) …; break;
//!     case"effort": case"model": case"max_thinking_tokens": case"flag_settings": break }
//!   return t }
//! ```
//!
//! LingXi builds its [`crate::PermissionPolicy`] ONCE at boot, is not `Clone`,
//! and lives behind a shared `Arc` — so before this module the port had no
//! per-call layer mechanism at all. Two of the ten upstream layer kinds were
//! open-coded as FLAT fields on
//! [`platform_api::permission_gate::PermissionCheckContext`]:
//! `mode_override` (the `permission_mode` layer) and `is_non_interactive_session`
//! (the `avoid_prompts` layer). This module generalizes both into the real
//! ordered fold, so the remaining eight kinds — in particular
//! `bash_command_clamp` — have somewhere to land.
//!
//! ## Where the fold is CALLED
//!
//! [`crate::PolicyPermissionGate::check_with_context_or_abort`] and
//! [`crate::PolicyPermissionGate::resolve_detailed_or_abort`] — the two
//! context-carrying dispatch chokepoints every subagent / main-loop tool call
//! goes through. Both convert `ctx.mode_override` + `ctx.is_non_interactive_session`
//! into head-of-list layers and append `ctx.permission_layers`, then fold.
//!
//! ## Wire shape
//!
//! [`platform_api::permission_gate::PermissionCheckContext::permission_layers`] carries
//! RAW `serde_json::Value` objects (the `platform-api` crate sits BELOW this one and
//! cannot name [`PermissionLayer`]; same precedent as
//! `PermissionCheckContext::permission_suggestions`). Each element is the
//! upstream discriminated-union shape, e.g.
//! `{"kind":"bash_command_clamp","rules":["Bash(git status:*)"]}`.

use crate::mode::PermissionMode;
use serde_json::Value;
use std::path::PathBuf;

/// One entry of upstream's `toolUseContext.permissionLayers` array.
///
/// All TEN upstream kinds are represented, including the four the permission
/// fold deliberately ignores (`effort` / `model` / `max_thinking_tokens` /
/// `flag_settings`) — they are still carried so
/// [`strip_widening_permission_layers`] can reproduce `qLa`'s keep/strip table
/// exactly, and so a round-trip through [`PermissionLayer::to_wire`] is lossless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionLayer {
    /// `allowed_tools` — extra `command` ALLOW rules (`ULa`).
    AllowedTools(Vec<String>),
    /// `disallowed_tools` — extra `command` DENY rules (`jLa`).
    DisallowedTools(Vec<String>),
    /// `bash_command_clamp` — ONE clamp GROUP. Every group must admit every
    /// command span, so groups intersect (see [`crate::bash_command_clamp`]).
    BashCommandClamp(Vec<String>),
    /// `avoid_prompts` — this agent cannot surface a permission prompt
    /// (`shouldAvoidPermissionPrompts`).
    AvoidPrompts,
    /// `permission_mode` — a per-call mode, in the WIRE spelling
    /// (`default`/`plan`/`acceptEdits`/`bypassPermissions`/`dontAsk`/`auto`).
    PermissionMode(String),
    /// `working_directory` — one extra working directory. Only the LAST such
    /// layer is applied (`n.findLast(...)`).
    WorkingDirectory(PathBuf),
    /// `effort` — ignored by the permission fold.
    Effort(String),
    /// `model` — ignored by the permission fold.
    Model(String),
    /// `max_thinking_tokens` — ignored by the permission fold.
    MaxThinkingTokens(u64),
    /// `flag_settings` — ignored by the permission fold.
    FlagSettings(Value),
}

impl PermissionLayer {
    /// The upstream `kind` discriminant.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::AllowedTools(_) => "allowed_tools",
            Self::DisallowedTools(_) => "disallowed_tools",
            Self::BashCommandClamp(_) => "bash_command_clamp",
            Self::AvoidPrompts => "avoid_prompts",
            Self::PermissionMode(_) => "permission_mode",
            Self::WorkingDirectory(_) => "working_directory",
            Self::Effort(_) => "effort",
            Self::Model(_) => "model",
            Self::MaxThinkingTokens(_) => "max_thinking_tokens",
            Self::FlagSettings(_) => "flag_settings",
        }
    }

    /// `pPS[kind] === "strip"` — is this layer WIDENING (droppable when a
    /// nested spawn must not inherit a loosened posture)?
    ///
    /// Oracle table (2.1.238 @287028951, `var pPS` in the `fu` module init):
    /// ```text
    /// allowed_tools:"strip", avoid_prompts:"strip", permission_mode:"strip",
    /// working_directory:"strip", disallowed_tools:"keep",
    /// bash_command_clamp:"keep", effort:"keep", model:"keep",
    /// max_thinking_tokens:"keep", flag_settings:"keep"
    /// ```
    #[must_use]
    pub fn is_widening(&self) -> bool {
        matches!(
            self,
            Self::AllowedTools(_)
                | Self::AvoidPrompts
                | Self::PermissionMode(_)
                | Self::WorkingDirectory(_)
        )
    }

    /// Parse one wire object. Returns `None` for an unknown/ malformed kind —
    /// upstream's `switch` simply has no arm for it, so an unrecognized layer is
    /// inert, never an error.
    #[must_use]
    pub fn from_wire(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let strings = |key: &str| -> Vec<String> {
            object
                .get(key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        match object.get("kind").and_then(Value::as_str)? {
            "allowed_tools" => Some(Self::AllowedTools(strings("allowedTools"))),
            "disallowed_tools" => Some(Self::DisallowedTools(strings("disallowedTools"))),
            "bash_command_clamp" => Some(Self::BashCommandClamp(strings("rules"))),
            "avoid_prompts" => Some(Self::AvoidPrompts),
            "permission_mode" => Some(Self::PermissionMode(
                object.get("mode").and_then(Value::as_str)?.to_string(),
            )),
            "working_directory" => Some(Self::WorkingDirectory(PathBuf::from(
                object.get("directory").and_then(Value::as_str)?,
            ))),
            "effort" => Some(Self::Effort(
                object.get("effort").and_then(Value::as_str)?.to_string(),
            )),
            "model" => Some(Self::Model(
                object
                    .get("mainLoopModel")
                    .and_then(Value::as_str)?
                    .to_string(),
            )),
            "max_thinking_tokens" => Some(Self::MaxThinkingTokens(
                object.get("maxThinkingTokens").and_then(Value::as_u64)?,
            )),
            "flag_settings" => Some(Self::FlagSettings(
                object.get("flagSettings").cloned().unwrap_or(Value::Null),
            )),
            _ => None,
        }
    }

    /// Render back to the wire object [`Self::from_wire`] accepts.
    #[must_use]
    pub fn to_wire(&self) -> Value {
        let kind = self.kind();
        match self {
            Self::AllowedTools(tools) => serde_json::json!({"kind": kind, "allowedTools": tools}),
            Self::DisallowedTools(tools) => {
                serde_json::json!({"kind": kind, "disallowedTools": tools})
            }
            Self::BashCommandClamp(rules) => serde_json::json!({"kind": kind, "rules": rules}),
            Self::AvoidPrompts => serde_json::json!({"kind": kind}),
            Self::PermissionMode(mode) => serde_json::json!({"kind": kind, "mode": mode}),
            Self::WorkingDirectory(dir) => {
                serde_json::json!({"kind": kind, "directory": dir.to_string_lossy()})
            }
            Self::Effort(effort) => serde_json::json!({"kind": kind, "effort": effort}),
            Self::Model(model) => serde_json::json!({"kind": kind, "mainLoopModel": model}),
            Self::MaxThinkingTokens(budget) => {
                serde_json::json!({"kind": kind, "maxThinkingTokens": budget})
            }
            Self::FlagSettings(settings) => {
                serde_json::json!({"kind": kind, "flagSettings": settings})
            }
        }
    }
}

/// Parse a raw wire array (as carried on
/// [`platform_api::permission_gate::PermissionCheckContext::permission_layers`]),
/// dropping entries no upstream `switch` arm would match.
#[must_use]
pub fn parse_permission_layers(wire: &[Value]) -> Vec<PermissionLayer> {
    wire.iter().filter_map(PermissionLayer::from_wire).collect()
}

/// `stripWideningPermissionLayers` (`qLa`, 2.1.238 @287028951):
/// `e.filter(t => pPS[t.kind] === "keep")`.
///
/// Used when a layer stack is inherited by a surface that must not be WIDENED
/// by it — the tightening kinds (`disallowed_tools`, `bash_command_clamp`) and
/// the permission-inert kinds survive; every loosening kind is dropped.
#[must_use]
pub fn strip_widening_permission_layers(layers: &[PermissionLayer]) -> Vec<PermissionLayer> {
    layers
        .iter()
        .filter(|layer| !layer.is_widening())
        .cloned()
        .collect()
}

/// `applyContextLayers` (`V3r`, 2.1.238 @287028951) — the PERMISSION half.
///
/// `V3r` appends `t` to `e.permissionLayers` and, additionally, hoists the LAST
/// `model` / `max_thinking_tokens` layer into `options.mainLoopModel` /
/// `options.thinkingConfig`. Those two hoists are model-config, not permission,
/// and belong to the orchestrator; this function ports the array append, which
/// is the part the permission fold reads.
#[must_use]
pub fn apply_context_layers(
    base: &[PermissionLayer],
    added: &[PermissionLayer],
) -> Vec<PermissionLayer> {
    if added.is_empty() {
        return base.to_vec();
    }
    let mut merged = base.to_vec();
    merged.extend(added.iter().cloned());
    merged
}

/// Session-level inputs the `permission_mode` arm of the fold consults —
/// upstream `gq()` (the settings bypass killswitch) and
/// `t.isBypassPermissionsModeAvailable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayerFoldInputs {
    /// `gq()` — `permissions.disableBypassPermissionsMode === "disable"`.
    pub bypass_killswitch_active: bool,
    /// `toolPermissionContext.isBypassPermissionsModeAvailable`.
    pub bypass_permissions_available: bool,
}

/// The result of folding a layer stack: the DELTA this call applies on top of
/// the session's boot/live permission state.
///
/// Every field is empty/`None` for an empty layer stack, and
/// [`Self::is_empty`] then reports `true` — which is how the gate keeps the
/// no-layer path byte-identical to its pre-layer behavior.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FoldedPermissionContext {
    /// The winning `permission_mode` layer (LAST one wins; a `bypassPermissions`
    /// layer is skipped when the killswitch is on or bypass is unavailable).
    pub mode: Option<PermissionMode>,
    /// Extra `command`-source ALLOW rule strings (`ULa`), deduped in order.
    pub allow_command_rules: Vec<String>,
    /// Extra `command`-source DENY rule strings (`jLa`), deduped in order.
    pub deny_command_rules: Vec<String>,
    /// Accumulated clamp groups, in layer order
    /// (`bashCommandClamps:[...t.bashCommandClamps??[], i.rules]`).
    pub bash_command_clamps: Vec<Vec<String>>,
    /// `shouldAvoidPermissionPrompts` — sticky-true.
    pub should_avoid_permission_prompts: bool,
    /// The LAST `working_directory` layer's directory (`n.findLast(...)`).
    pub additional_working_directory: Option<PathBuf>,
}

impl FoldedPermissionContext {
    /// Does this fold change anything at all? (The gate skips the whole
    /// policy-clone path when it does not.)
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mode.is_none()
            && self.allow_command_rules.is_empty()
            && self.deny_command_rules.is_empty()
            && self.bash_command_clamps.is_empty()
            && !self.should_avoid_permission_prompts
            && self.additional_working_directory.is_none()
    }
}

/// `gn(toolUseContext)` — fold an ORDERED layer stack into the per-call delta.
///
/// Faithful to the oracle in the three places order matters:
///
/// * `permission_mode` — later layers overwrite earlier ones, EXCEPT a
///   `bypassPermissions` layer that the killswitch or an unavailable bypass
///   grant rejects: that layer `break`s WITHOUT overwriting, so an earlier
///   layer's mode survives (it is not downgraded to `default`).
/// * `working_directory` — only the layer identical to `findLast(...)` applies,
///   i.e. exactly one directory regardless of how many such layers exist.
/// * `avoid_prompts` — sticky-true, never cleared.
///
/// `allowed_tools` / `disallowed_tools` accumulate through `to(...)` (upstream's
/// unique-preserving concat), reproduced here as order-preserving dedup.
///
/// NOT modeled: the `pollEventDeliveryGuard` pre-pass (`fPS`), which strips
/// dangerous allow rules before the walk and suppresses the `allowed_tools`
/// arm. That guard belongs to the Poll tool, which is out of this port's scope.
#[must_use]
pub fn fold_permission_layers(
    layers: &[PermissionLayer],
    inputs: LayerFoldInputs,
) -> FoldedPermissionContext {
    let mut folded = FoldedPermissionContext::default();
    if layers.is_empty() {
        return folded;
    }
    // `let o = n.findLast(i => i.kind === "working_directory")`
    let last_working_dir = layers.iter().rev().find_map(|layer| match layer {
        PermissionLayer::WorkingDirectory(dir) => Some(dir.clone()),
        _ => None,
    });
    for layer in layers {
        match layer {
            PermissionLayer::AllowedTools(tools) => {
                push_unique(&mut folded.allow_command_rules, tools);
            }
            PermissionLayer::DisallowedTools(tools) => {
                // `jLa` short-circuits on an EMPTY list, so an empty
                // `disallowed_tools` layer is a no-op rather than a rewrite.
                if !tools.is_empty() {
                    push_unique(&mut folded.deny_command_rules, tools);
                }
            }
            PermissionLayer::BashCommandClamp(rules) => {
                folded.bash_command_clamps.push(rules.clone());
            }
            PermissionLayer::AvoidPrompts => {
                folded.should_avoid_permission_prompts = true;
            }
            PermissionLayer::PermissionMode(mode) => {
                let is_bypass = mode == "bypassPermissions";
                if is_bypass
                    && (inputs.bypass_killswitch_active || !inputs.bypass_permissions_available)
                {
                    continue;
                }
                if let Some(parsed) = layer_mode_from_wire(mode) {
                    folded.mode = Some(parsed);
                }
            }
            PermissionLayer::WorkingDirectory(dir) => {
                if last_working_dir.as_ref() == Some(dir) {
                    folded.additional_working_directory = Some(dir.clone());
                }
            }
            PermissionLayer::Effort(_)
            | PermissionLayer::Model(_)
            | PermissionLayer::MaxThinkingTokens(_)
            | PermissionLayer::FlagSettings(_) => {}
        }
    }
    folded
}

/// Wire mode spelling → [`PermissionMode`]. `None` for an unrecognized value:
/// upstream stores the raw string, and every downstream mode `switch` falls
/// through, so an unknown mode must NOT rewrite the session mode.
#[must_use]
pub fn layer_mode_from_wire(mode: &str) -> Option<PermissionMode> {
    match mode {
        "default" | "manual" => Some(PermissionMode::Default),
        "plan" => Some(PermissionMode::Plan),
        "acceptEdits" => Some(PermissionMode::AcceptEdits),
        "bypassPermissions" => Some(PermissionMode::BypassPermissions),
        "dontAsk" => Some(PermissionMode::DontAsk),
        "auto" => Some(PermissionMode::Auto),
        _ => None,
    }
}

/// `to([...existing, ...added])` — concat then unique, preserving first-seen order.
fn push_unique(target: &mut Vec<String>, added: &[String]) {
    for item in added {
        if !target.iter().any(|existing| existing == item) {
            target.push(item.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clamp(rules: &[&str]) -> PermissionLayer {
        PermissionLayer::BashCommandClamp(rules.iter().map(|r| (*r).to_string()).collect())
    }

    #[test]
    fn empty_stack_folds_to_nothing() {
        let folded = fold_permission_layers(&[], LayerFoldInputs::default());
        assert!(folded.is_empty());
    }

    #[test]
    fn clamp_layers_accumulate_in_order() {
        let layers = vec![clamp(&["Bash(git status:*)"]), clamp(&["Bash(ls:*)"])];
        let folded = fold_permission_layers(&layers, LayerFoldInputs::default());
        assert_eq!(
            folded.bash_command_clamps,
            vec![
                vec!["Bash(git status:*)".to_string()],
                vec!["Bash(ls:*)".to_string()]
            ]
        );
        assert!(!folded.is_empty());
    }

    #[test]
    fn last_permission_mode_layer_wins() {
        let layers = vec![
            PermissionLayer::PermissionMode("plan".into()),
            PermissionLayer::PermissionMode("acceptEdits".into()),
        ];
        let folded = fold_permission_layers(&layers, LayerFoldInputs::default());
        assert_eq!(folded.mode, Some(PermissionMode::AcceptEdits));
    }

    #[test]
    fn bypass_layer_is_skipped_when_bypass_is_unavailable() {
        // `if(i.mode==="bypassPermissions"&&(gq()||!t.isBypassPermissionsModeAvailable))break`
        // — the layer is SKIPPED, so the earlier `plan` layer still stands. It is
        // NOT downgraded to `default`.
        let layers = vec![
            PermissionLayer::PermissionMode("plan".into()),
            PermissionLayer::PermissionMode("bypassPermissions".into()),
        ];
        let folded = fold_permission_layers(&layers, LayerFoldInputs::default());
        assert_eq!(folded.mode, Some(PermissionMode::Plan));
    }

    #[test]
    fn bypass_layer_applies_when_available_and_not_killswitched() {
        let layers = vec![PermissionLayer::PermissionMode("bypassPermissions".into())];
        let folded = fold_permission_layers(
            &layers,
            LayerFoldInputs {
                bypass_killswitch_active: false,
                bypass_permissions_available: true,
            },
        );
        assert_eq!(folded.mode, Some(PermissionMode::BypassPermissions));
    }

    #[test]
    fn bypass_layer_is_skipped_by_the_settings_killswitch() {
        let layers = vec![PermissionLayer::PermissionMode("bypassPermissions".into())];
        let folded = fold_permission_layers(
            &layers,
            LayerFoldInputs {
                bypass_killswitch_active: true,
                bypass_permissions_available: true,
            },
        );
        assert_eq!(folded.mode, None);
    }

    #[test]
    fn only_the_last_working_directory_layer_applies() {
        let layers = vec![
            PermissionLayer::WorkingDirectory(PathBuf::from("/a")),
            PermissionLayer::WorkingDirectory(PathBuf::from("/b")),
        ];
        let folded = fold_permission_layers(&layers, LayerFoldInputs::default());
        assert_eq!(
            folded.additional_working_directory,
            Some(PathBuf::from("/b"))
        );
    }

    #[test]
    fn avoid_prompts_is_sticky() {
        let layers = vec![
            PermissionLayer::AvoidPrompts,
            PermissionLayer::PermissionMode("plan".into()),
        ];
        let folded = fold_permission_layers(&layers, LayerFoldInputs::default());
        assert!(folded.should_avoid_permission_prompts);
    }

    #[test]
    fn tool_lists_dedupe_in_first_seen_order() {
        let layers = vec![
            PermissionLayer::AllowedTools(vec!["Bash(ls:*)".into(), "Read".into()]),
            PermissionLayer::AllowedTools(vec!["Read".into(), "Glob".into()]),
        ];
        let folded = fold_permission_layers(&layers, LayerFoldInputs::default());
        assert_eq!(
            folded.allow_command_rules,
            vec![
                "Bash(ls:*)".to_string(),
                "Read".to_string(),
                "Glob".to_string()
            ]
        );
    }

    #[test]
    fn empty_disallowed_tools_layer_is_a_no_op() {
        // `jLa` returns `e` untouched when `t.length===0`.
        let layers = vec![PermissionLayer::DisallowedTools(Vec::new())];
        let folded = fold_permission_layers(&layers, LayerFoldInputs::default());
        assert!(folded.is_empty());
    }

    #[test]
    fn model_config_layers_do_not_touch_the_permission_fold() {
        let layers = vec![
            PermissionLayer::Effort("high".into()),
            PermissionLayer::Model("some-model".into()),
            PermissionLayer::MaxThinkingTokens(0),
            PermissionLayer::FlagSettings(serde_json::json!({"a": 1})),
        ];
        let folded = fold_permission_layers(&layers, LayerFoldInputs::default());
        assert!(folded.is_empty());
    }

    #[test]
    fn strip_widening_keeps_exactly_the_oracle_keep_set() {
        let layers = vec![
            PermissionLayer::AllowedTools(vec!["Bash".into()]),
            PermissionLayer::AvoidPrompts,
            PermissionLayer::PermissionMode("plan".into()),
            PermissionLayer::WorkingDirectory(PathBuf::from("/a")),
            PermissionLayer::DisallowedTools(vec!["Bash(rm:*)".into()]),
            clamp(&["Bash(ls:*)"]),
            PermissionLayer::Effort("high".into()),
            PermissionLayer::Model("m".into()),
            PermissionLayer::MaxThinkingTokens(1),
            PermissionLayer::FlagSettings(Value::Null),
        ];
        let kept: Vec<&str> = strip_widening_permission_layers(&layers)
            .iter()
            .map(PermissionLayer::kind)
            .collect();
        assert_eq!(
            kept,
            vec![
                "disallowed_tools",
                "bash_command_clamp",
                "effort",
                "model",
                "max_thinking_tokens",
                "flag_settings",
            ]
        );
    }

    #[test]
    fn apply_context_layers_appends_in_order() {
        let base = vec![PermissionLayer::PermissionMode("plan".into())];
        let added = vec![clamp(&["Bash(ls:*)"])];
        let merged = apply_context_layers(&base, &added);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[1].kind(), "bash_command_clamp");
        // Empty additions return the base untouched (`if(t.length===0)return e`).
        assert_eq!(apply_context_layers(&base, &[]), base);
    }

    #[test]
    fn wire_round_trip_is_lossless_for_every_kind() {
        let layers = vec![
            PermissionLayer::AllowedTools(vec!["Bash(ls:*)".into()]),
            PermissionLayer::DisallowedTools(vec!["Bash(rm:*)".into()]),
            clamp(&["Bash(git status:*)"]),
            PermissionLayer::AvoidPrompts,
            PermissionLayer::PermissionMode("dontAsk".into()),
            PermissionLayer::WorkingDirectory(PathBuf::from("/tmp/x")),
            PermissionLayer::Effort("low".into()),
            PermissionLayer::Model("m".into()),
            PermissionLayer::MaxThinkingTokens(4096),
            PermissionLayer::FlagSettings(serde_json::json!({"k": true})),
        ];
        let wire: Vec<Value> = layers.iter().map(PermissionLayer::to_wire).collect();
        assert_eq!(parse_permission_layers(&wire), layers);
    }

    #[test]
    fn unknown_layer_kinds_are_inert_not_errors() {
        let wire = vec![
            serde_json::json!({"kind": "brand_new_kind", "x": 1}),
            serde_json::json!({"kind": "bash_command_clamp", "rules": ["Bash(ls:*)"]}),
        ];
        let parsed = parse_permission_layers(&wire);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].kind(), "bash_command_clamp");
    }

    #[test]
    fn unknown_permission_mode_does_not_rewrite_the_session_mode() {
        let layers = vec![
            PermissionLayer::PermissionMode("plan".into()),
            PermissionLayer::PermissionMode("bubble".into()),
        ];
        let folded = fold_permission_layers(&layers, LayerFoldInputs::default());
        assert_eq!(folded.mode, Some(PermissionMode::Plan));
    }
}
