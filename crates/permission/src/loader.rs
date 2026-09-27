//! Permission loader — projects a settings file's `permissions` block into a
//! `Vec<PermissionRule>` tagged with its source.
//!
//! Mirrors the hooks loader (`lingxi-hooks::parse_hooks_from_settings_json`): a
//! best-effort projection from raw settings JSON, NOT a strict validator. 1:1
//! with claude-code `settingsJsonToRules` (`utils/permissions/permissionsLoader.ts`):
//! the `permissions.{allow,deny,ask}` string arrays each become rules with the
//! corresponding [`PermissionBehavior`], all tagged with the caller-supplied
//! [`PermissionRuleSource`]. Each spec string is parsed via
//! [`PermissionRuleValue::from_rule_string`].
//!
//! ## Scope (parity phase 1 — foundation, no enforcement)
//! This is the pure load step. It does NOT wire a gate or read files at boot —
//! the enforcing gate ([`crate::PermissionPolicy`] + a `PermissionGate` impl)
//! and the boot-time multi-tier disk read are later phases. `defaultMode` and
//! `additionalDirectories` from the block are not consumed here (mode resolution
//! is a gate/boot concern).
//!
//! Settings shape (claude-code compatible — `settings.json`):
//! ```json
//! { "permissions": { "allow": ["Bash(npm run *)"], "deny": ["Read(./secrets/**)"], "ask": [] } }
//! ```

use crate::mode::PermissionMode;
use crate::rule::{PermissionBehavior, PermissionRule, PermissionRuleSource, PermissionRuleValue};
use serde::Deserialize;

/// Startup warning for a permission rule whose tool has NO file-permission
/// matcher of its own — parity 2.1.210 (`validatePermissionRule` / `EPr`'s final
/// `valid:!0, warning:…` branch, added in 2.1.210). `Write`, `NotebookEdit`, and
/// `MultiEdit` share the `Edit(path)` matcher; `Glob` shares the `Read(path)`
/// matcher — so a `Write(src/**)` rule silently matches nothing. When such a rule
/// carries a real path content (NOT a Bash-style `:*` prefix), the binary keeps
/// the rule (`valid:!0`) but surfaces this warning at startup, steering the user
/// to the covering `Edit(path)` / `Read(path)` rule.
///
/// Byte-locked to the binary template:
/// ``${sp(o)} is not matched by file permission checks — only ${a}(path) rules
/// are. Use ${sp({toolName:a,ruleContent:o.ruleContent})} instead (${a} rules
/// cover all file-${a==="Edit"?"editing":"reading"} tools).`` where `sp` is
/// [`PermissionRuleValue::to_rule_string`] and `a` is the covering tool.
///
/// Returns `None` for tools with their own matcher (`Edit`/`Read`/`Bash`/…), for
/// bare tool-wide rules (`rule_content == None`), and — matching the binary's
/// `!o.ruleContent.includes(":*")` guard — for any content carrying a `:*`
/// prefix (`EPr`'s earlier `qvl` branch already errors on `:*` for these file
/// tools, so the warning branch never sees it). Behavior-independent: the binary
/// runs this branch for allow, deny, and ask rules alike.
#[must_use]
pub fn permission_rule_file_warning(value: &PermissionRuleValue) -> Option<String> {
    let content = value.rule_content.as_deref()?;
    let covering = match value.tool_name.as_str() {
        "Write" | "NotebookEdit" | "MultiEdit" => "Edit",
        "Glob" => "Read",
        _ => return None,
    };
    if content.contains(":*") {
        return None;
    }
    let file_verb = if covering == "Edit" {
        "editing"
    } else {
        "reading"
    };
    let suggested = PermissionRuleValue {
        tool_name: covering.to_string(),
        rule_content: Some(content.to_string()),
    };
    Some(format!(
        "{} is not matched by file permission checks — only {covering}(path) rules are. \
         Use {} instead ({covering} rules cover all file-{file_verb} tools).",
        value.to_rule_string(),
        suggested.to_rule_string(),
    ))
}

/// Startup warning for an allow-listed Bash rule whose wildcard appears in a
/// token before a later fixed subcommand/argument token. Such a wildcard also
/// matches options inserted before the intended subcommand/argument. This
/// mirrors the validator's token walk: options and shell operators are not
/// fixed tokens, the first ordinary token without a preceding wildcard ends
/// the scan, and a trailing `:*` prefix rule is excluded. Escaped `\*` is
/// literal, and a wildcard in the final token is the intended suffix form.
fn permission_rule_bash_wildcard_warning(
    value: &PermissionRuleValue,
    behavior: PermissionBehavior,
) -> Option<String> {
    if value.tool_name != "Bash" || !matches!(behavior, PermissionBehavior::Allow) {
        return None;
    }
    let content = value.rule_content.as_deref()?;
    if content.ends_with(":*") {
        return None;
    }
    let mut tokens = content.split_whitespace();
    // The validator only considers patterns with a command and at least two
    // following tokens (and leaves `Bash(* main)` silent).
    let command = tokens.next()?;
    if contains_unescaped_wildcard(command) {
        return None;
    }

    let mut token_count = 1;
    let mut saw_wildcard = false;
    for token in tokens {
        token_count += 1;
        // The oracle aborts on shell operators/redirections: those tokens do
        // not establish a fixed subcommand/argument after the wildcard.
        if is_shell_operator_or_redirection(token) {
            return None;
        }
        if contains_unescaped_wildcard(token) {
            saw_wildcard = true;
            continue;
        }
        // Option tokens may occur after a wildcard without being the fixed
        // subcommand/argument that makes the wildcard dangerous.
        if token.starts_with('-') {
            continue;
        }
        return if saw_wildcard && token_count >= 3 {
            Some(bash_wildcard_warning(value, command))
        } else {
            None
        };
    }
    None
}

fn bash_wildcard_warning(value: &PermissionRuleValue, command: &str) -> String {
    let mut warning = format!(
        "{} has a wildcard before the rest of the command, so it also matches any options inserted at that position and approves them without a prompt.",
        value.to_rule_string()
    );
    if command == "git" {
        warning.push_str(
            " For git, options such as -c and --exec-path can run arbitrary commands. Replace that * with the exact value you mean, or only use * after the subcommand (for example Bash(git status *)).",
        );
    } else {
        warning.push_str(
            " Replace that * with the exact value you mean, or only use * after the subcommand.",
        );
    }
    warning
}

/// Matches the validator's shell-operator/redirection check (`[|&;<>]` or a
/// numeric file-descriptor prefix followed by `<`/`>`).
fn is_shell_operator_or_redirection(token: &str) -> bool {
    let bytes = token.as_bytes();
    if bytes
        .first()
        .is_some_and(|byte| matches!(byte, b'|' | b'&' | b';' | b'<' | b'>'))
    {
        return true;
    }
    let mut digits = 0;
    while digits < bytes.len() && bytes[digits].is_ascii_digit() {
        digits += 1;
    }
    digits > 0 && digits < bytes.len() && matches!(bytes[digits], b'<' | b'>')
}

/// Returns whether `value` contains an unescaped `*`. Backslashes are counted
/// by parity so `\*` is literal while `\\*` has an active wildcard.
fn contains_unescaped_wildcard(value: &str) -> bool {
    let mut escaped = false;
    for byte in value.bytes() {
        if byte == b'\\' {
            escaped = !escaped;
            continue;
        }
        if byte == b'*' && !escaped {
            return true;
        }
        escaped = false;
    }
    false
}

/// Top-level projection consumed by [`permission_rules_from_settings_json`].
/// A separate private struct (like the hooks loader) so this loader stays
/// decoupled from the engine's typed `SettingsJson`.
#[derive(Debug, Deserialize)]
struct SettingsTop {
    #[serde(default)]
    permissions: Option<PermissionsBlock>,
    /// TOP-LEVEL managed-settings lockdown flag (a sibling of `permissions`,
    /// NOT inside it — claude-code schema: `allowManagedPermissionRulesOnly:
    /// E.boolean().optional()`). Only meaningful when set in the managed
    /// (policySettings) tier; consumed via
    /// [`allow_managed_permission_rules_only_from_settings_json`].
    #[serde(default, rename = "allowManagedPermissionRulesOnly")]
    allow_managed_permission_rules_only: Option<bool>,
    /// TOP-LEVEL `disableAutoMode` killswitch. claude-code's schema declares
    /// `disableAutoMode: E.enum(["disable"]).optional()` at BOTH the top level
    /// AND inside `permissions` (`Bpa()` checks both positions); this is the
    /// top-level sibling. Consumed via [`auto_mode_disabled_from_settings_json`].
    #[serde(default, rename = "disableAutoMode")]
    disable_auto_mode: Option<String>,
    /// TOP-LEVEL `skipDangerousModePermissionPrompt` — set true once the user has
    /// accepted the Bypass Permissions disclaimer interactively (claude-code
    /// `Pq()` reads it at the top level of each settings tier). Consumed via
    /// [`skip_dangerous_mode_permission_prompt_from_settings_json`].
    #[serde(default, rename = "skipDangerousModePermissionPrompt")]
    skip_dangerous_mode_permission_prompt: Option<bool>,
    /// TOP-LEVEL `autoMode` object. claude-code schema declares
    /// `autoMode: E.object({ classifyAllShell: E.boolean().optional()… }).optional()`
    /// as a sibling of `permissions`; `QOi()` reads `Pr(tier)?.autoMode?.
    /// classifyAllShell === true`. Consumed via
    /// [`classify_all_shell_from_settings_json`].
    #[serde(default, rename = "autoMode")]
    auto_mode: Option<AutoModeBlock>,
}

/// The `autoMode` settings object. Only [`Self::classify_all_shell`] is read by
/// this port; other keys are tolerated-and-ignored.
#[derive(Debug, Default, Deserialize)]
struct AutoModeBlock {
    /// `autoMode.classifyAllShell`: when `true`, every `Bash`/`PowerShell` allow
    /// rule is suspended while auto mode is active so all shell commands route
    /// through the classifier.
    #[serde(default, rename = "classifyAllShell")]
    classify_all_shell: Option<bool>,
}

/// The `permissions` block. `allow`/`deny`/`ask` are arrays of rule strings;
/// `defaultMode` selects the mode-fallback for unmatched calls.
#[derive(Debug, Default, Deserialize)]
struct PermissionsBlock {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default)]
    ask: Vec<String>,
    #[serde(default, rename = "defaultMode")]
    default_mode: Option<String>,
    #[serde(default, rename = "disableBypassPermissionsMode")]
    disable_bypass_permissions_mode: Option<String>,
    /// `permissions.disableAutoMode` — the auto-mode killswitch at its
    /// permissions-block position (`Bpa()`'s `e.permissions?.disableAutoMode`).
    #[serde(default, rename = "disableAutoMode")]
    disable_auto_mode: Option<String>,
    /// Extra directories (beyond cwd) inside which `acceptEdits`/auto-allow
    /// treats edits as writable. 1:1 with claude-code `permissions.additionalDirectories`,
    /// which `TGd` folds into `ToolPermissionContext.additionalWorkingDirectories`
    /// and `b$` unions with cwd for the `kF` working-dir auto-allow check.
    #[serde(default, rename = "additionalDirectories")]
    additional_directories: Vec<String>,
    /// `permissions.blockReadsOutsideWorkingDirectories` (2.1.263). Oracle
    /// schema text: *"Refuse file-tool reads (Read, Grep, Glob, LSP) outside the
    /// working directories in every permission mode; **true in any settings
    /// source wins**. Also set when the user picks "block" on the one-time
    /// auto-mode prompt for a read outside the working directories."*
    ///
    /// Managed policy carries it as a `restrictive` entry
    /// (`{path:["permissions","blockReadsOutsideWorkingDirectories"], restrictive:true}`),
    /// and the managed merge is `if (… === true) out = true` — an OR, never a
    /// last-writer-wins overwrite. See
    /// [`block_reads_outside_working_directories_from_settings_json`].
    #[serde(default, rename = "blockReadsOutsideWorkingDirectories")]
    block_reads_outside_working_directories: Option<bool>,
}

/// The full startup warning LINE for a rule that carries a
/// [`permission_rule_file_warning`] or Bash wildcard warning, prefixed exactly like the binary:
/// ``Permission ${ruleBehavior} rule (${sourceDisplay}): ${warning}``. Returns
/// `None` when the rule needs no warning. `source_display` is the caller-resolved
/// origin label — the binary uses the settings file path for the on-disk tiers
/// (`mwo`), `"managed policy settings"` for `policySettings`, `--allowed-tools` /
/// `--disallowed-tools` for `cliArg`, and `--settings` for `flagSettings`. The
/// `ruleBehavior` token is the lowercase wire name (`allow` / `deny` / `ask`).
///
/// Mirrors the per-rule guard the binary's startup loop (`sks`) applies BEFORE
/// running `validatePermissionRule`: a rule whose content is an
/// `identifier:...`-shaped prefix (a colon at index > 0 whose leading segment is
/// a bare identifier) is skipped — UNLESS the trimmed content is a Windows drive
/// path (`C:\…` / `C:/…`), which is a real file path and so still warns. This
/// suppresses the file-matcher warning for prefix-shaped contents such as
/// `Write(scheme:foo)` exactly as the binary does. (The narrower `:*` Bash-prefix
/// case is already handled inside [`permission_rule_file_warning`].) Allow+Bash
/// rules with a wildcard before a later fixed token receive the corresponding
/// command-pattern warning; the rule remains valid and is not removed.
#[must_use]
pub fn permission_rule_startup_warning(
    rule: &PermissionRule,
    source_display: &str,
) -> Option<String> {
    // `sks` loop guard: skip `identifier:`-prefixed contents (non-Windows-drive).
    if let Some(content) = rule.value.rule_content.as_deref() {
        if is_identifier_colon_prefix(content) {
            return None;
        }
    }
    let warning = permission_rule_file_warning(&rule.value)
        .or_else(|| permission_rule_bash_wildcard_warning(&rule.value, rule.behavior))?;
    let behavior = match rule.behavior {
        PermissionBehavior::Allow => "allow",
        PermissionBehavior::Deny => "deny",
        PermissionBehavior::Ask => "ask",
    };
    Some(format!(
        "Permission {behavior} rule ({source_display}): {warning}"
    ))
}

/// `true` when `content` is an `identifier:...`-shaped prefix that the binary's
/// startup loop skips before warning — 1:1 with the `sks` guard
/// `X>0 && /^[A-Za-z_][A-Za-z0-9_]*$/.test(G.slice(0,X).trim()) &&
/// !/^[A-Za-z]:[\\/]/.test(G.trim())`, where `X = G.indexOf(":")`. A Windows
/// drive path (`C:\…` / `C:/…`) is a real path, so it is NOT skipped (returns
/// `false`) and still warns.
fn is_identifier_colon_prefix(content: &str) -> bool {
    // `X = G.indexOf(":")`, then require `X > 0`.
    let Some(colon) = content.find(':') else {
        return false;
    };
    if colon == 0 {
        return false;
    }
    // `G.slice(0, X).trim()` must be a bare identifier `[A-Za-z_][A-Za-z0-9_]*`.
    let prefix = content[..colon].trim();
    let mut chars = prefix.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    // …unless `G.trim()` is a Windows drive path `^[A-Za-z]:[\\/]` (still a path).
    let trimmed = content.trim().as_bytes();
    let is_windows_drive = trimmed.len() >= 3
        && trimmed[0].is_ascii_alphabetic()
        && trimmed[1] == b':'
        && (trimmed[2] == b'\\' || trimmed[2] == b'/');
    !is_windows_drive
}

/// Parse one settings file's raw JSON into permission rules tagged with
/// `source`. Returns `Ok(vec![])` when there is no `permissions` block.
///
/// # Errors
/// Returns the `serde_json::Error` if `raw` is not valid JSON. (A file that is
/// valid JSON but has no `permissions` block is not an error — it yields an
/// empty vec, matching the best-effort hooks-loader contract.)
pub fn permission_rules_from_settings_json(
    raw: &str,
    source: PermissionRuleSource,
) -> Result<Vec<PermissionRule>, serde_json::Error> {
    let top: SettingsTop = serde_json::from_str(raw)?;
    let Some(block) = top.permissions else {
        return Ok(Vec::new());
    };
    // Deny/allow/ask order is cosmetic here (the buckets are evaluated by
    // `PermissionPolicy::authorize`, deny-first); we just project every spec.
    let mut out = Vec::with_capacity(block.allow.len() + block.deny.len() + block.ask.len());
    for (behavior, specs) in [
        (PermissionBehavior::Deny, &block.deny),
        (PermissionBehavior::Allow, &block.allow),
        (PermissionBehavior::Ask, &block.ask),
    ] {
        for spec in specs {
            out.push(PermissionRule {
                value: PermissionRuleValue::from_rule_string(spec),
                behavior,
                source,
            });
        }
    }
    Ok(out)
}

/// Parse `permissions.defaultMode` into a [`PermissionMode`] (claude-code wire
/// names: `default` / `plan` / `acceptEdits` / `bypassPermissions` / `dontAsk`).
/// Returns `None` when the block, the field, or the value is absent/unrecognized
/// (the caller falls back to [`PermissionMode::Default`]).
///
/// `"manual"` is accepted as an alias for `"default"` (parity 2.1.207): the
/// binary's zod schema declares `defaultMode:
/// E.preprocess(ZS, E.enum([...]))` where `ZS(e)=e==="manual"?"default":e`
/// normalizes the value BEFORE the enum validation, and the field's `.describe`
/// text reads "'manual' is accepted as an alias for 'default'". Without this
/// arm a tier whose `defaultMode` is `"manual"` returns `None` and is skipped
/// by the multi-tier reader, letting a lower-priority tier win.
#[must_use]
pub fn default_mode_from_settings_json(raw: &str) -> Option<PermissionMode> {
    let top: SettingsTop = serde_json::from_str(raw).ok()?;
    match top.permissions?.default_mode?.as_str() {
        "default" | "manual" => Some(PermissionMode::Default),
        "plan" => Some(PermissionMode::Plan),
        "acceptEdits" => Some(PermissionMode::AcceptEdits),
        "bypassPermissions" => Some(PermissionMode::BypassPermissions),
        "dontAsk" => Some(PermissionMode::DontAsk),
        // #32: claude-code's settings `defaultMode` enum includes "auto"
        // (`E.enum(["default","acceptEdits","bypassPermissions","plan","dontAsk",
        // "auto"])`). Accepted at parse; the actual runtime ENTRY into auto-mode
        // is further gated (model gate + circuit-breaker + disableAutoMode) by
        // [`crate::auto_gate`] — the boot mode-load path applies
        // [`crate::auto_gate::apply_auto_mode_gate`], which downgrades `auto` to
        // `default` when the gate is closed (claude-code `xms`).
        "auto" => Some(PermissionMode::Auto),
        _ => None,
    }
}

/// `MODE-SETTINGS-AUTO-TRUST-01` / 2.1.257 `C(e)`: may a settings tier of this
/// `source` GRANT a trust-gated `defaultMode` (`"auto"` or `"bypassPermissions"`)?
///
/// claude-code's `le=["policySettings","flagSettings","userSettings"]` is the
/// same list for both: `C(e){return F().some((o)=>fe(o)?.permissions?.defaultMode===e)}`.
/// A `defaultMode` of `"auto"` or `"bypassPermissions"` from `projectSettings`
/// or `localSettings` is IGNORED (warn +
/// `tengu_settings_{auto,bypass}_mode_untrusted_source_ignored`) because those
/// files are repo-controllable.
///
/// Returns `true` for the trusted tiers (User/Policy/Flag) and `false` for the
/// repo-controllable ones (Project/Local) and the runtime tiers
/// (CliArg/Command/Session), which never carry a settings `defaultMode`.
/// Plan / acceptEdits / dontAsk / default may still be set from any tier.
#[must_use]
pub fn auto_mode_grantable_by_source(source: PermissionRuleSource) -> bool {
    matches!(
        source,
        PermissionRuleSource::Settings(protocol::SettingsScope::User)
            | PermissionRuleSource::Settings(protocol::SettingsScope::Managed)
            | PermissionRuleSource::FlagSettings
    )
}

/// 2.1.257: may this settings *source* apply `defaultMode` of `mode`?
///
/// Auto (2.1.211) and bypassPermissions (2.1.257) require
/// [`auto_mode_grantable_by_source`]. Other modes apply from any tier.
#[must_use]
pub fn default_mode_applies_from_source(
    mode: crate::mode::PermissionMode,
    source: PermissionRuleSource,
) -> bool {
    match mode {
        crate::mode::PermissionMode::Auto | crate::mode::PermissionMode::BypassPermissions => {
            auto_mode_grantable_by_source(source)
        }
        _ => true,
    }
}

/// Byte-exact 2.1.257 warn when an untrusted tier's `defaultMode: "bypassPermissions"`
/// is dropped. Em-dash is ASCII `--` in the oracle's `—` (U+2014).
pub const UNTRUSTED_BYPASS_DEFAULT_MODE_WARN: &str = "settings defaultMode \"bypassPermissions\" ignored \u{2014} only policy/user/flag settings may grant bypass mode (projectSettings and localSettings are repo-controllable)";

/// Byte-exact warn when an untrusted tier's `defaultMode: "auto"` is dropped.
pub const UNTRUSTED_AUTO_DEFAULT_MODE_WARN: &str = "settings defaultMode \"auto\" ignored \u{2014} only policy/user/flag settings may grant auto mode (projectSettings and localSettings are repo-controllable)";

/// Does this settings file DISABLE `bypassPermissions` mode? True iff
/// `permissions.disableBypassPermissionsMode == "disable"` (claude-code's
/// bypass-permissions killswitch). When any tier disables it, the constructed
/// [`crate::PermissionPolicy`]'s `bypass_killswitch_active` is set so
/// `authorize` falls back to `Ask` even in `BypassPermissions` mode.
#[must_use]
pub fn bypass_permissions_disabled_from_settings_json(raw: &str) -> bool {
    serde_json::from_str::<SettingsTop>(raw)
        .ok()
        .and_then(|t| t.permissions)
        .and_then(|p| p.disable_bypass_permissions_mode)
        .as_deref()
        == Some("disable")
}

/// Does this settings file DISABLE auto mode? True iff `disableAutoMode ==
/// "disable"` at EITHER position — top-level `disableAutoMode` OR
/// `permissions.disableAutoMode` — 1:1 with claude-code's `Bpa()`
/// (`e.disableAutoMode==="disable"||e.permissions?.disableAutoMode==="disable"`).
///
/// The `disableAutoMode` schema is `E.enum(["disable"]).optional()` at both
/// positions, so `"disable"` is the only meaningful value. When any settings
/// tier disables it, [`crate::PermissionPolicy::auto_mode_disabled`] is set so
/// both the boot mode-load gate ([`crate::auto_gate::apply_auto_mode_gate`]) and
/// the live `set_permission_mode` gate refuse `auto`.
#[must_use]
pub fn auto_mode_disabled_from_settings_json(raw: &str) -> bool {
    let Ok(top) = serde_json::from_str::<SettingsTop>(raw) else {
        return false;
    };
    if top.disable_auto_mode.as_deref() == Some("disable") {
        return true;
    }
    top.permissions.and_then(|p| p.disable_auto_mode).as_deref() == Some("disable")
}

/// Does this settings file ENABLE the `autoMode.classifyAllShell` escalation?
/// True iff `autoMode.classifyAllShell === true` — 1:1 with the per-tier read
/// inside claude-code's `QOi()`
/// (`Pr(tier)?.autoMode?.classifyAllShell===!0`). Strictly `true` (a missing key
/// or a non-boolean value is `false`), matching the `===!0` comparison.
///
/// The engine resolves the session-wide flag as the STICKY OR over every settings
/// tier (any tier enabling it wins — `QOi` returns on the first `true`), then
/// feeds it to [`crate::PermissionPolicy::with_classify_all_shell`]. When set,
/// every `Bash`/`PowerShell` allow rule is suspended in auto mode so all shell
/// commands route through the classifier. Returns `false` when the block, the
/// field, or the JSON is absent/invalid (best-effort projection, like the other
/// loaders here).
#[must_use]
pub fn classify_all_shell_from_settings_json(raw: &str) -> bool {
    serde_json::from_str::<SettingsTop>(raw)
        .ok()
        .and_then(|t| t.auto_mode)
        .and_then(|a| a.classify_all_shell)
        == Some(true)
}

/// `MODE-BG-DISCLAIMER-02`: does this settings file set
/// `skipDangerousModePermissionPrompt` truthy at the TOP LEVEL?
///
/// 1:1 with claude-code `Pq()`'s per-tier
/// `getSettings(tier)?.skipDangerousModePermissionPrompt` read. The flag is
/// persisted once the user accepts the Bypass Permissions disclaimer
/// interactively; when ANY tier has it set, a background session's requested
/// `bypassPermissions` is NOT downgraded (the disclaimer was already accepted).
/// Returns `false` when the field or the JSON is absent/invalid (best-effort;
/// failing closed here means the bg gate MAY trip, which over-asks safely).
#[must_use]
pub fn skip_dangerous_mode_permission_prompt_from_settings_json(raw: &str) -> bool {
    serde_json::from_str::<SettingsTop>(raw)
        .ok()
        .and_then(|t| t.skip_dangerous_mode_permission_prompt)
        .unwrap_or(false)
}

/// Parse `permissions.additionalDirectories` (a string array of extra working
/// directories) from one settings file's raw JSON. These are the dirs beyond
/// `cwd` inside which `acceptEdits` mode auto-allows safe file edits.
///
/// 1:1 with claude-code: `TGd(s, settings.permissions?.additionalDirectories, …)`
/// folds each entry into `ToolPermissionContext.additionalWorkingDirectories`,
/// which `b$(e)=new Set([cwd(),...e.additionalWorkingDirectories.keys()])` unions
/// with the cwd to form the `kF` working-dir auto-allow set.
///
/// Returns `Vec::new()` when the block, the field, or the JSON is absent/invalid
/// (best-effort projection, like the other loaders here). The returned paths are
/// the RAW settings strings as [`PathBuf`]s (relative / `~`-prefixed / absolute) —
/// they are resolved against the policy's filesystem roots at authorize time by
/// `expand_path`, so the caller need not pre-resolve them. Entries containing a
/// NUL byte are discarded because they cannot be represented as filesystem
/// paths by downstream consumers.
#[must_use]
pub fn additional_directories_from_settings_json(raw: &str) -> Vec<std::path::PathBuf> {
    serde_json::from_str::<SettingsTop>(raw)
        .ok()
        .and_then(|t| t.permissions)
        .map(|p| {
            p.additional_directories
                .into_iter()
                .filter(|path| !path.contains('\0'))
                .filter(|path| !crate::working_dirs::is_network_working_directory(path))
                .map(std::path::PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// `permissions.blockReadsOutsideWorkingDirectories` for ONE settings source.
///
/// Returns `true` only for a literal JSON `true`. Callers must fold the tiers
/// with OR — the oracle's managed merge is
/// `if (e.permissions.blockReadsOutsideWorkingDirectories === !0) d.… = !0`, so
/// **`true` in any source wins** and a later `false` cannot clear it. Use
/// [`fold_block_reads_outside_working_directories`] rather than re-implementing
/// the fold per host.
#[must_use]
pub fn block_reads_outside_working_directories_from_settings_json(raw: &str) -> bool {
    serde_json::from_str::<SettingsTop>(raw)
        .ok()
        .and_then(|t| t.permissions)
        .and_then(|p| p.block_reads_outside_working_directories)
        == Some(true)
}

/// Fold `permissions.blockReadsOutsideWorkingDirectories` across every settings
/// source: `true` in ANY source wins (oracle OR-merge, never last-wins).
#[must_use]
pub fn fold_block_reads_outside_working_directories<'a>(
    raws: impl IntoIterator<Item = &'a str>,
) -> bool {
    raws.into_iter()
        .any(block_reads_outside_working_directories_from_settings_json)
}

/// Does this settings file set the managed-only permission-rule lockdown?
/// True iff the TOP-LEVEL `allowManagedPermissionRulesOnly` is exactly `true`.
///
/// 1:1 with claude-code `$wt()` (`wr("policySettings")?.
/// allowManagedPermissionRulesOnly===!0`): when ANY managed tier sets it true
/// (`u.some((g)=>g.allowManagedPermissionRulesOnly===!0)` in the managed-tier
/// fold), `RKt()` returns ONLY the policySettings rules — schema text: "When
/// true (and set in managed settings), only permission rules (allow/deny/ask)
/// from managed settings are respected. User, project, local, and CLI argument
/// permission rules are ignored." The caller applies the retain; this helper is
/// the pure per-file parse (best-effort like the other loaders here).
#[must_use]
pub fn allow_managed_permission_rules_only_from_settings_json(raw: &str) -> bool {
    serde_json::from_str::<SettingsTop>(raw)
        .ok()
        .and_then(|t| t.allow_managed_permission_rules_only)
        == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_mode_grantable_only_from_trusted_tiers() {
        // `le` / `C(e)`: policy/user/flag may grant auto AND bypassPermissions.
        assert!(auto_mode_grantable_by_source(
            PermissionRuleSource::Settings(protocol::SettingsScope::User)
        ));
        assert!(auto_mode_grantable_by_source(
            PermissionRuleSource::Settings(protocol::SettingsScope::Managed)
        ));
        assert!(auto_mode_grantable_by_source(
            PermissionRuleSource::FlagSettings
        ));
        // Repo-controllable tiers may NOT.
        assert!(!auto_mode_grantable_by_source(
            PermissionRuleSource::Settings(protocol::SettingsScope::Project)
        ));
        assert!(!auto_mode_grantable_by_source(
            PermissionRuleSource::Settings(protocol::SettingsScope::Local)
        ));
        // Runtime tiers never carry a settings defaultMode.
        assert!(!auto_mode_grantable_by_source(PermissionRuleSource::CliArg));
        assert!(!auto_mode_grantable_by_source(
            PermissionRuleSource::Command
        ));
        assert!(!auto_mode_grantable_by_source(
            PermissionRuleSource::Session
        ));
    }

    #[test]
    fn default_mode_applies_from_source_trust_gates_auto_and_bypass_only() {
        use crate::mode::PermissionMode;
        let project = PermissionRuleSource::Settings(protocol::SettingsScope::Project);
        let user = PermissionRuleSource::Settings(protocol::SettingsScope::User);
        assert!(!default_mode_applies_from_source(
            PermissionMode::Auto,
            project
        ));
        assert!(!default_mode_applies_from_source(
            PermissionMode::BypassPermissions,
            project
        ));
        assert!(default_mode_applies_from_source(
            PermissionMode::BypassPermissions,
            user
        ));
        assert!(default_mode_applies_from_source(
            PermissionMode::Plan,
            project
        ));
        assert!(default_mode_applies_from_source(
            PermissionMode::AcceptEdits,
            project
        ));
    }

    #[test]
    fn untrusted_bypass_warn_is_byte_exact() {
        assert_eq!(
            UNTRUSTED_BYPASS_DEFAULT_MODE_WARN,
            "settings defaultMode \"bypassPermissions\" ignored \u{2014} only policy/user/flag settings may grant bypass mode (projectSettings and localSettings are repo-controllable)"
        );
        assert_eq!(
            UNTRUSTED_AUTO_DEFAULT_MODE_WARN,
            "settings defaultMode \"auto\" ignored \u{2014} only policy/user/flag settings may grant auto mode (projectSettings and localSettings are repo-controllable)"
        );
    }

    #[test]
    fn classify_all_shell_reads_auto_mode_block_strictly() {
        // AUTO-03 / `QOi`: only `autoMode.classifyAllShell === true` enables it.
        assert!(classify_all_shell_from_settings_json(
            r#"{"autoMode": {"classifyAllShell": true}}"#
        ));
        // Strict `=== true`: false, a truthy non-bool, or a missing key ⇒ false.
        assert!(!classify_all_shell_from_settings_json(
            r#"{"autoMode": {"classifyAllShell": false}}"#
        ));
        assert!(!classify_all_shell_from_settings_json(
            r#"{"autoMode": {"classifyAllShell": 1}}"#
        ));
        assert!(!classify_all_shell_from_settings_json(
            r#"{"autoMode": {"classifyAllShell": "true"}}"#
        ));
        assert!(!classify_all_shell_from_settings_json(
            r#"{"autoMode": {}}"#
        ));
        // The key lives at the TOP LEVEL `autoMode`, NOT under `permissions`.
        assert!(!classify_all_shell_from_settings_json(
            r#"{"permissions": {"autoMode": {"classifyAllShell": true}}}"#
        ));
        // Absent / empty / malformed ⇒ false (best-effort, like the siblings).
        assert!(!classify_all_shell_from_settings_json("{}"));
        assert!(!classify_all_shell_from_settings_json("not json"));
    }

    #[test]
    fn skip_dangerous_mode_permission_prompt_reads_top_level_flag() {
        // MODE-BG-DISCLAIMER-02: top-level truthy flag.
        assert!(skip_dangerous_mode_permission_prompt_from_settings_json(
            r#"{"skipDangerousModePermissionPrompt": true}"#
        ));
        assert!(!skip_dangerous_mode_permission_prompt_from_settings_json(
            r#"{"skipDangerousModePermissionPrompt": false}"#
        ));
        // Absent / empty / malformed → false (fail-closed = bg gate may trip).
        assert!(!skip_dangerous_mode_permission_prompt_from_settings_json(
            "{}"
        ));
        assert!(!skip_dangerous_mode_permission_prompt_from_settings_json(
            "not json"
        ));
    }

    #[test]
    fn no_permissions_block_is_empty() {
        assert!(permission_rules_from_settings_json(
            "{}",
            PermissionRuleSource::Settings(protocol::SettingsScope::User)
        )
        .unwrap()
        .is_empty());
        // Other settings keys present, but no permissions → still empty.
        let raw = r#"{ "model": "claude-opus-4-7" }"#;
        assert!(permission_rules_from_settings_json(
            raw,
            PermissionRuleSource::Settings(protocol::SettingsScope::Project)
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn invalid_json_is_err() {
        assert!(permission_rules_from_settings_json(
            "{not json",
            PermissionRuleSource::Settings(protocol::SettingsScope::User)
        )
        .is_err());
    }

    #[test]
    fn projects_allow_deny_ask_with_source_and_behavior() {
        let raw = r#"{
            "permissions": {
                "allow": ["Bash(npm run *)", "Read"],
                "deny": ["Read(./secrets/**)"],
                "ask": ["WebFetch"]
            }
        }"#;
        let rules = permission_rules_from_settings_json(
            raw,
            PermissionRuleSource::Settings(protocol::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(rules.len(), 4);
        // Every rule carries the caller's source.
        assert!(rules
            .iter()
            .all(|r| r.source == PermissionRuleSource::Settings(protocol::SettingsScope::Project)));

        let find = |tool: &str, content: Option<&str>| {
            rules
                .iter()
                .find(|r| r.value.tool_name == tool && r.value.rule_content.as_deref() == content)
        };
        // allow → parsed rule_content.
        let bash = find("Bash", Some("npm run *")).expect("Bash(npm run *)");
        assert!(matches!(bash.behavior, PermissionBehavior::Allow));
        // bare allow.
        assert!(matches!(
            find("Read", None).expect("Read").behavior,
            PermissionBehavior::Allow
        ));
        // deny.
        assert!(matches!(
            find("Read", Some("./secrets/**"))
                .expect("deny Read")
                .behavior,
            PermissionBehavior::Deny
        ));
        // ask.
        assert!(matches!(
            find("WebFetch", None).expect("ask WebFetch").behavior,
            PermissionBehavior::Ask
        ));
    }

    #[test]
    fn empty_arrays_yield_no_rules() {
        let raw = r#"{ "permissions": { "allow": [], "deny": [], "ask": [] } }"#;
        assert!(permission_rules_from_settings_json(
            raw,
            PermissionRuleSource::Settings(protocol::SettingsScope::User)
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn bypass_killswitch_parses() {
        let f = bypass_permissions_disabled_from_settings_json;
        assert!(f(
            r#"{ "permissions": { "disableBypassPermissionsMode": "disable" } }"#
        ));
        assert!(!f(
            r#"{ "permissions": { "disableBypassPermissionsMode": "enable" } }"#
        ));
        assert!(!f(r#"{ "permissions": {} }"#));
        assert!(!f("{}"));
        assert!(!f("not json"));
    }

    #[test]
    fn auto_mode_killswitch_parses_both_positions() {
        let f = auto_mode_disabled_from_settings_json;
        // TOP-LEVEL position.
        assert!(f(r#"{ "disableAutoMode": "disable" }"#));
        // permissions-block position.
        assert!(f(r#"{ "permissions": { "disableAutoMode": "disable" } }"#));
        // Both set → still true.
        assert!(f(
            r#"{ "disableAutoMode": "disable", "permissions": { "disableAutoMode": "disable" } }"#
        ));
        // Coexists with other keys.
        assert!(f(
            r#"{ "permissions": { "disableAutoMode": "disable", "allow": ["Read"] } }"#
        ));
        // Only "disable" counts (enum(["disable"])); anything else / absent → false.
        assert!(!f(r#"{ "disableAutoMode": "enable" }"#));
        assert!(!f(r#"{ "permissions": { "disableAutoMode": "" } }"#));
        assert!(!f(r#"{ "permissions": {} }"#));
        assert!(!f("{}"));
        assert!(!f("not json"));
    }

    #[test]
    fn default_mode_parses_wire_names() {
        let m = |raw: &str| default_mode_from_settings_json(raw);
        assert!(matches!(
            m(r#"{ "permissions": { "defaultMode": "dontAsk" } }"#),
            Some(PermissionMode::DontAsk)
        ));
        assert!(matches!(
            m(r#"{ "permissions": { "defaultMode": "acceptEdits" } }"#),
            Some(PermissionMode::AcceptEdits)
        ));
        assert!(matches!(
            m(r#"{ "permissions": { "defaultMode": "bypassPermissions" } }"#),
            Some(PermissionMode::BypassPermissions)
        ));
        // #32: "auto" is an accepted settings defaultMode value (was dropped→None).
        assert!(matches!(
            m(r#"{ "permissions": { "defaultMode": "auto" } }"#),
            Some(PermissionMode::Auto)
        ));
        // parity 2.1.207: "manual" is an alias for "default" (ZS preprocess).
        // Must map to Default (NOT None) so the tier is not skipped.
        assert!(matches!(
            m(r#"{ "permissions": { "defaultMode": "manual" } }"#),
            Some(PermissionMode::Default)
        ));
        // Absent / no block / unknown → None (caller defaults to Default).
        assert!(m(r#"{ "permissions": {} }"#).is_none());
        assert!(m("{}").is_none());
        assert!(m(r#"{ "permissions": { "defaultMode": "bogus" } }"#).is_none());
    }

    /// Regression for the parity-2.1.207 `manual`-alias tier bug: the CLI's
    /// multi-tier reader (`read_cli_mode_settings`) applies `if let Some(m) =
    /// default_mode_from_settings_json(..)` per tier, project last (highest
    /// priority). Before the fix a project `defaultMode:"manual"` returned
    /// `None`, so it was SKIPPED and a lower-priority user tier (`plan`) won.
    /// After the fix `manual` → `Some(Default)`, so the project tier wins.
    #[test]
    fn manual_project_tier_beats_lower_priority_user_tier() {
        let user = r#"{ "permissions": { "defaultMode": "plan" } }"#;
        let project = r#"{ "permissions": { "defaultMode": "manual" } }"#;
        // Mirror the reader loop: user first, then project (project last wins).
        let mut default_mode: Option<PermissionMode> = None;
        for raw in [user, project] {
            if let Some(m) = default_mode_from_settings_json(raw) {
                default_mode = Some(m);
            }
        }
        assert_eq!(
            default_mode,
            Some(PermissionMode::Default),
            "project defaultMode:manual must beat user defaultMode:plan"
        );
    }

    #[test]
    fn additional_directories_parse() {
        use std::path::PathBuf;
        let f = additional_directories_from_settings_json;
        // Present → returned as raw PathBufs (relative / ~ / absolute preserved).
        assert_eq!(
            f(
                r#"{ "permissions": { "additionalDirectories": ["../sibling", "~/work", "/abs/dir"] } }"#
            ),
            vec![
                PathBuf::from("../sibling"),
                PathBuf::from("~/work"),
                PathBuf::from("/abs/dir"),
            ]
        );
        // Absent field / no block / no JSON → empty (best-effort).
        assert!(f(r#"{ "permissions": { "allow": ["Bash"] } }"#).is_empty());
        assert!(f(r#"{ "permissions": {} }"#).is_empty());
        assert!(f("{}").is_empty());
        assert!(f("not json").is_empty());
        // Coexists with other permissions keys.
        assert_eq!(
            f(r#"{ "permissions": { "allow": ["Read"], "additionalDirectories": ["a"] } }"#),
            vec![PathBuf::from("a")]
        );
        // A NUL byte cannot be represented by downstream filesystem paths, so
        // discard only that entry while preserving valid entries and order.
        assert_eq!(
            f(
                r#"{ "permissions": { "additionalDirectories": ["../sibling", "bad\u0000path", "~/work"] } }"#
            ),
            vec![PathBuf::from("../sibling"), PathBuf::from("~/work")]
        );
        // UNC / `/net/<host>` are refused before they become working dirs.
        assert_eq!(
            f(
                r#"{ "permissions": { "additionalDirectories": ["../sibling", "//fileserver/share", "/net/host/data", "/abs/dir"] } }"#
            ),
            vec![PathBuf::from("../sibling"), PathBuf::from("/abs/dir")]
        );
    }

    #[test]
    fn allow_managed_permission_rules_only_parses_top_level() {
        let f = allow_managed_permission_rules_only_from_settings_json;
        // TOP-LEVEL true → lockdown.
        assert!(f(r#"{ "allowManagedPermissionRulesOnly": true }"#));
        // Coexists with a permissions block.
        assert!(f(
            r#"{ "allowManagedPermissionRulesOnly": true, "permissions": { "deny": ["Bash(rm:*)"] } }"#
        ));
        // Exactly-true semantics (`===!0`): false / absent / wrong type → off.
        assert!(!f(r#"{ "allowManagedPermissionRulesOnly": false }"#));
        assert!(!f("{}"));
        assert!(!f("not json"));
        // INSIDE `permissions` is the WRONG place (schema keeps it top-level) —
        // must not trigger the lockdown.
        assert!(!f(
            r#"{ "permissions": { "allowManagedPermissionRulesOnly": true } }"#
        ));
    }

    /// The caller-side retain (mirroring claude-code `RKt()` under `$wt()`):
    /// when the lockdown is set, only `PolicySettings`-sourced rules survive.
    #[test]
    fn managed_only_lockdown_retain_drops_non_managed_rules() {
        let user = permission_rules_from_settings_json(
            r#"{ "permissions": { "allow": ["WebFetch"] } }"#,
            PermissionRuleSource::Settings(protocol::SettingsScope::User),
        )
        .unwrap();
        let managed = permission_rules_from_settings_json(
            r#"{ "permissions": { "deny": ["Bash(rm:*)"] } }"#,
            PermissionRuleSource::Settings(protocol::SettingsScope::Managed),
        )
        .unwrap();
        let mut rules: Vec<PermissionRule> = user.into_iter().chain(managed).collect();
        assert_eq!(rules.len(), 2);
        rules.retain(|r| {
            r.source == PermissionRuleSource::Settings(protocol::SettingsScope::Managed)
        });
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].value.tool_name, "Bash");
        assert!(matches!(rules[0].behavior, PermissionBehavior::Deny));
    }

    #[test]
    fn file_warning_covers_write_family_and_glob() {
        let warn =
            |spec: &str| permission_rule_file_warning(&PermissionRuleValue::from_rule_string(spec));
        // parity 2.1.210: Write/NotebookEdit/MultiEdit steer to Edit(path).
        assert_eq!(
            warn("Write(src/foo.ts)").as_deref(),
            Some(
                "Write(src/foo.ts) is not matched by file permission checks — only Edit(path) rules are. Use Edit(src/foo.ts) instead (Edit rules cover all file-editing tools)."
            )
        );
        assert_eq!(
            warn("NotebookEdit(nb.ipynb)").as_deref(),
            Some(
                "NotebookEdit(nb.ipynb) is not matched by file permission checks — only Edit(path) rules are. Use Edit(nb.ipynb) instead (Edit rules cover all file-editing tools)."
            )
        );
        assert_eq!(
            warn("MultiEdit(src/**)").as_deref(),
            Some(
                "MultiEdit(src/**) is not matched by file permission checks — only Edit(path) rules are. Use Edit(src/**) instead (Edit rules cover all file-editing tools)."
            )
        );
        // Glob steers to Read(path) with the "reading" verb.
        assert_eq!(
            warn("Glob(**/*.rs)").as_deref(),
            Some(
                "Glob(**/*.rs) is not matched by file permission checks — only Read(path) rules are. Use Read(**/*.rs) instead (Read rules cover all file-reading tools)."
            )
        );
    }

    #[test]
    fn startup_warning_line_matches_binary_prefix() {
        let rule = PermissionRule {
            value: PermissionRuleValue::from_rule_string("Write(src/foo.ts)"),
            behavior: PermissionBehavior::Allow,
            source: PermissionRuleSource::Settings(protocol::SettingsScope::Project),
        };
        assert_eq!(
            permission_rule_startup_warning(&rule, ".lingxi/settings.json").as_deref(),
            Some(
                "Permission allow rule (.lingxi/settings.json): Write(src/foo.ts) is not matched by file permission checks — only Edit(path) rules are. Use Edit(src/foo.ts) instead (Edit rules cover all file-editing tools)."
            )
        );
        // Deny behavior surfaces the `deny` token; managed display label.
        let deny = PermissionRule {
            value: PermissionRuleValue::from_rule_string("Glob(**/*.rs)"),
            behavior: PermissionBehavior::Deny,
            source: PermissionRuleSource::Settings(protocol::SettingsScope::Managed),
        };
        assert_eq!(
            permission_rule_startup_warning(&deny, "managed policy settings").as_deref(),
            Some(
                "Permission deny rule (managed policy settings): Glob(**/*.rs) is not matched by file permission checks — only Read(path) rules are. Use Read(**/*.rs) instead (Read rules cover all file-reading tools)."
            )
        );
        // A covered rule produces no line at all.
        let ok = PermissionRule {
            value: PermissionRuleValue::from_rule_string("Edit(src/foo.ts)"),
            behavior: PermissionBehavior::Allow,
            source: PermissionRuleSource::Settings(protocol::SettingsScope::User),
        };
        assert!(permission_rule_startup_warning(&ok, "settings.json").is_none());
    }

    #[test]
    fn startup_warning_flags_bash_wildcard_before_subcommand_exactly() {
        let rule = PermissionRule {
            value: PermissionRuleValue::from_rule_string("Bash(git -C * status *)"),
            behavior: PermissionBehavior::Allow,
            source: PermissionRuleSource::Settings(protocol::SettingsScope::Project),
        };
        assert_eq!(
            permission_rule_startup_warning(&rule, ".lingxi/settings.json").as_deref(),
            Some(
                "Permission allow rule (.lingxi/settings.json): Bash(git -C * status *) has a wildcard before the rest of the command, so it also matches any options inserted at that position and approves them without a prompt. For git, options such as -c and --exec-path can run arbitrary commands. Replace that * with the exact value you mean, or only use * after the subcommand (for example Bash(git status *))."
            )
        );

        // The reviewer-reduced shape has the same exact warning body.
        let reduced = PermissionRule {
            value: PermissionRuleValue::from_rule_string("Bash(git * main)"),
            behavior: PermissionBehavior::Allow,
            source: PermissionRuleSource::Settings(protocol::SettingsScope::User),
        };
        assert_eq!(
            permission_rule_startup_warning(&reduced, "settings.json").as_deref(),
            Some(
                "Permission allow rule (settings.json): Bash(git * main) has a wildcard before the rest of the command, so it also matches any options inserted at that position and approves them without a prompt. For git, options such as -c and --exec-path can run arbitrary commands. Replace that * with the exact value you mean, or only use * after the subcommand (for example Bash(git status *))."
            )
        );
    }

    #[test]
    fn startup_warning_bash_wildcard_checks_all_settings_tier_prefixes() {
        let displays = [
            (
                PermissionRuleSource::Settings(protocol::SettingsScope::User),
                "user settings",
            ),
            (
                PermissionRuleSource::Settings(protocol::SettingsScope::Project),
                "project settings",
            ),
            (
                PermissionRuleSource::Settings(protocol::SettingsScope::Local),
                "local settings",
            ),
            (PermissionRuleSource::CliArg, "CLI argument"),
            (
                PermissionRuleSource::Settings(protocol::SettingsScope::Managed),
                "managed policy settings",
            ),
        ];
        for (source, display) in displays {
            let rule = PermissionRule {
                value: PermissionRuleValue::from_rule_string("Bash(git * main)"),
                behavior: PermissionBehavior::Allow,
                source,
            };
            let warning = permission_rule_startup_warning(&rule, display)
                .expect("wildcard warning should be visible for every startup tier");
            assert!(
                warning.starts_with(&format!(
                    "Permission allow rule ({display}): Bash(git * main)"
                )),
                "warning={warning}"
            );
        }
    }

    #[test]
    fn startup_warning_ignores_safe_bash_wildcard_shapes() {
        let warning = |spec: &str, behavior: PermissionBehavior| {
            let rule = PermissionRule {
                value: PermissionRuleValue::from_rule_string(spec),
                behavior,
                source: PermissionRuleSource::Settings(protocol::SettingsScope::Project),
            };
            permission_rule_startup_warning(&rule, "settings.json")
        };

        // A final wildcard is the intended suffix form; no fixed token follows
        // a wildcard in either pattern.
        assert!(warning("Bash(git status *)", PermissionBehavior::Allow).is_none());
        assert!(warning("Bash(git status * main)", PermissionBehavior::Allow).is_none());
        assert!(warning("Bash(git *)", PermissionBehavior::Allow).is_none());
        assert!(warning("Bash(git * *)", PermissionBehavior::Allow).is_none());
        assert!(warning("Bash(git * --literal-option)", PermissionBehavior::Allow).is_none());
        assert!(warning("Bash(git * > output)", PermissionBehavior::Allow).is_none());
        assert!(warning("Bash(git * status:*)", PermissionBehavior::Allow).is_none());
        // An escaped star is a literal, not a permission wildcard. A wildcard
        // embedded at the end of a token likewise has no later fixed token.
        assert!(warning(r"Bash(git \* main)", PermissionBehavior::Allow).is_none());
        assert!(warning("Bash(git *foo)", PermissionBehavior::Allow).is_none());
        assert!(warning("Bash(git foo*)", PermissionBehavior::Allow).is_none());
        // Only allow+Bash rules use this startup warning.
        assert!(warning("Bash(git * main)", PermissionBehavior::Deny).is_none());
        assert!(warning("Bash(git * main)", PermissionBehavior::Ask).is_none());
        assert!(warning("Read(git * main)", PermissionBehavior::Allow).is_none());
    }

    #[test]
    fn startup_warning_skips_identifier_colon_prefix_like_binary() {
        let line = |spec: &str| {
            let rule = PermissionRule {
                value: PermissionRuleValue::from_rule_string(spec),
                behavior: PermissionBehavior::Allow,
                source: PermissionRuleSource::Settings(protocol::SettingsScope::Project),
            };
            permission_rule_startup_warning(&rule, "settings.json")
        };
        // `sks` loop guard: `identifier:...` (non-`:*`, non-drive) content is
        // skipped BEFORE the file-matcher warning — the binary stays silent.
        assert!(line("Write(scheme:foo)").is_none());
        assert!(line("Glob(node:fs)").is_none());
        // A Windows drive path is a real path → NOT skipped, still warns.
        assert!(line(r"Write(C:\Users\x)").is_some());
        assert!(line("Write(C:/Users/x)").is_some());
        // A non-identifier prefix before the colon does NOT match the guard.
        assert!(line("Write(a-b:c)").is_some());
        // No colon → warns as before (unaffected by the guard).
        assert!(line("Write(src/**)").is_some());
    }

    #[test]
    fn file_warning_absent_for_covered_and_bare_and_prefix_rules() {
        let warn =
            |spec: &str| permission_rule_file_warning(&PermissionRuleValue::from_rule_string(spec));
        // Tools with their OWN matcher never warn.
        assert!(warn("Edit(src/foo.ts)").is_none());
        assert!(warn("Read(secret.env)").is_none());
        assert!(warn("Bash(npm run:*)").is_none());
        // Bare tool-wide rule (rule_content None) → no warning.
        assert!(warn("Write").is_none());
        assert!(warn("Write(*)").is_none());
        // `:*` prefix content is handled by EPr's earlier error branch, not the
        // warning branch (binary guard `!ruleContent.includes(":*")`).
        assert!(warn("Write(foo:*)").is_none());
    }

    #[test]
    fn missing_arrays_default_empty() {
        // Only `allow` present; `deny`/`ask` default to [].
        let raw = r#"{ "permissions": { "allow": ["Bash"] } }"#;
        let rules = permission_rules_from_settings_json(raw, PermissionRuleSource::CliArg).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].value.tool_name, "Bash");
    }
}
