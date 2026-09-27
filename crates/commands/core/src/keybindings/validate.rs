//! Keybinding validation — a 1:1 port of
//! `claude-code/src/keybindings/validate.ts`.
//!
//! Validation NEVER rejects the whole file: it produces a list of
//! [`KeybindingWarning`]s (errors + warnings) which the loader surfaces
//! alongside the (still-merged) bindings — exactly like the TS, where the loader
//! returns `{ bindings, warnings }` and never throws on bad blocks.

use super::parser::{chord_to_string, parse_chord, parse_keystroke};
use super::reserved::{get_reserved_shortcuts_for, normalize_key_for_comparison, Severity};
use super::schema::is_valid_context;
use super::types::{KeybindingBlock, ParsedBinding};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// 1:1 with `plural` (utils/stringUtils.ts:32): `n === 1 ? word : word+'s'`.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// Kinds of validation issue. 1:1 with the TS `KeybindingWarningType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeybindingWarningType {
    /// Structural / parse problem.
    ParseError,
    /// A duplicate key within a context.
    Duplicate,
    /// A reserved / OS-intercepted shortcut.
    Reserved,
    /// An unknown context name.
    InvalidContext,
    /// A malformed action value.
    InvalidAction,
}

/// A warning or error about a keybinding configuration issue.
/// 1:1 with the TS `KeybindingWarning`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingWarning {
    /// The category of the issue.
    pub r#type: KeybindingWarningType,
    /// Error vs. warning.
    pub severity: Severity,
    /// Human-readable message.
    pub message: String,
    /// The offending key chord, if any.
    pub key: Option<String>,
    /// The context, if known.
    pub context: Option<String>,
    /// The offending action, if any.
    pub action: Option<String>,
    /// A remediation suggestion, if any.
    pub suggestion: Option<String>,
}

impl KeybindingWarning {
    fn new(r#type: KeybindingWarningType, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            r#type,
            severity,
            message: message.into(),
            key: None,
            context: None,
            action: None,
            suggestion: None,
        }
    }
}

/// The `command:` binding format regex. 1:1 with `^command:[a-zA-Z0-9:\-_]+$`.
fn command_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^command:[a-zA-Z0-9:\-_]+$").expect("static regex"))
}

/// Validate a single keystroke string. 1:1 with `validateKeystroke`
/// (validate.ts:91-125).
fn validate_keystroke(keystroke: &str) -> Option<KeybindingWarning> {
    for part in keystroke.to_lowercase().split('+') {
        if part.trim().is_empty() {
            let mut w = KeybindingWarning::new(
                KeybindingWarningType::ParseError,
                Severity::Error,
                format!("Empty key part in \"{keystroke}\""),
            );
            w.key = Some(keystroke.to_string());
            w.suggestion = Some("Remove extra \"+\" characters".to_string());
            return Some(w);
        }
    }

    let parsed = parse_keystroke(keystroke);
    if parsed.key.is_empty() && !parsed.ctrl && !parsed.alt && !parsed.shift && !parsed.meta {
        // NOTE: the TS checks `!parsed.key && !ctrl && !alt && !shift && !meta`
        // (super deliberately NOT included — match validate.ts:109-114).
        let mut w = KeybindingWarning::new(
            KeybindingWarningType::ParseError,
            Severity::Error,
            format!("Could not parse keystroke \"{keystroke}\""),
        );
        w.key = Some(keystroke.to_string());
        return Some(w);
    }
    None
}

/// A raw block as it appears in the user JSON, before structural validation.
/// Mirrors the `unknown` the TS `validateBlock` inspects: a context that may be
/// missing/invalid and a `key → value` bindings map where the value may be a
/// non-string. We model the "non-string action" case with an explicit flag.
#[derive(Debug, Clone)]
pub struct RawBlock {
    /// The context string, or `None` if absent / non-string.
    pub context: Option<String>,
    /// `true` if a `context` field was present but not a string (TS
    /// `typeof rawContext !== 'string'` → "missing context" parse error).
    pub context_missing: bool,
    /// The raw bindings: key → (action string or `None` for null) plus a flag
    /// for "value was neither string nor null".
    pub bindings: Vec<RawBinding>,
    /// `true` if the `bindings` field was absent / not an object.
    pub bindings_missing: bool,
}

/// One raw `key: value` entry from a user block.
#[derive(Debug, Clone)]
pub struct RawBinding {
    /// The key chord string.
    pub key: String,
    /// The action: `Some(s)` for a string, `None` for an explicit `null`.
    pub action: Option<String>,
    /// `true` when the JSON value was neither a string nor `null` (the TS
    /// "Invalid action ... must be a string or null" branch).
    pub non_string: bool,
}

/// Validate a single raw block. 1:1 with `validateBlock` (validate.ts:130-247).
// Faithful straight-line port of a ~120-line TS function; the per-field warning
// fill assigns cloned context/action onto freshly-`None` warning fields.
#[allow(clippy::too_many_lines, clippy::assigning_clones)]
fn validate_block(block: &RawBlock, block_index: usize) -> Vec<KeybindingWarning> {
    let mut warnings = Vec::new();

    // Context.
    let mut context_name: Option<String> = None;
    if block.context_missing || block.context.is_none() {
        warnings.push(KeybindingWarning::new(
            KeybindingWarningType::ParseError,
            Severity::Error,
            format!(
                "Keybinding block {} missing \"context\" field",
                block_index + 1
            ),
        ));
    } else {
        let raw = block.context.as_deref().unwrap_or("");
        if is_valid_context(raw) {
            context_name = Some(raw.to_string());
        } else {
            let mut w = KeybindingWarning::new(
                KeybindingWarningType::InvalidContext,
                Severity::Error,
                format!("Unknown context \"{raw}\""),
            );
            w.context = Some(raw.to_string());
            w.suggestion = Some(format!(
                "Valid contexts: {}",
                super::schema::KEYBINDING_CONTEXTS.join(", ")
            ));
            warnings.push(w);
        }
    }

    // Bindings field.
    if block.bindings_missing {
        warnings.push(KeybindingWarning::new(
            KeybindingWarningType::ParseError,
            Severity::Error,
            format!(
                "Keybinding block {} missing \"bindings\" field",
                block_index + 1
            ),
        ));
        return warnings;
    }

    for rb in &block.bindings {
        // Key syntax.
        if let Some(mut key_err) = validate_keystroke(&rb.key) {
            key_err.context = context_name.clone();
            warnings.push(key_err);
        }

        // Action value.
        if rb.non_string {
            let mut w = KeybindingWarning::new(
                KeybindingWarningType::InvalidAction,
                Severity::Error,
                format!(
                    "Invalid action for \"{}\": must be a string or null",
                    rb.key
                ),
            );
            w.key = Some(rb.key.clone());
            w.context = context_name.clone();
            warnings.push(w);
        } else if let Some(action) = &rb.action {
            if action.starts_with("command:") {
                if !command_regex().is_match(action) {
                    let mut w = KeybindingWarning::new(
                        KeybindingWarningType::InvalidAction,
                        Severity::Warning,
                        format!(
                            "Invalid command binding \"{action}\" for \"{}\": command name may only contain alphanumeric characters, colons, hyphens, and underscores",
                            rb.key
                        ),
                    );
                    w.key = Some(rb.key.clone());
                    w.context = context_name.clone();
                    w.action = Some(action.clone());
                    warnings.push(w);
                }
                if let Some(ctx) = &context_name {
                    if ctx != "Chat" {
                        let mut w = KeybindingWarning::new(
                            KeybindingWarningType::InvalidAction,
                            Severity::Warning,
                            format!(
                                "Command binding \"{action}\" must be in \"Chat\" context, not \"{ctx}\""
                            ),
                        );
                        w.key = Some(rb.key.clone());
                        w.context = context_name.clone();
                        w.action = Some(action.clone());
                        w.suggestion = Some(
                            "Move this binding to a block with \"context\": \"Chat\"".to_string(),
                        );
                        warnings.push(w);
                    }
                }
            } else if action == "voice:pushToTalk" {
                // Bare-letter push-to-talk warning (validate.ts:220-242).
                if let Some(ks) = parse_chord(&rb.key).into_iter().next() {
                    let bare_letter = ks.key.len() == 1
                        && ks
                            .key
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_lowercase());
                    if !ks.ctrl && !ks.alt && !ks.shift && !ks.meta && !ks.super_ && bare_letter {
                        let mut w = KeybindingWarning::new(
                            KeybindingWarningType::InvalidAction,
                            Severity::Warning,
                            format!(
                                "Binding \"{}\" to voice:pushToTalk prints into the input during warmup; use space or a modifier combo like meta+k",
                                rb.key
                            ),
                        );
                        w.key = Some(rb.key.clone());
                        w.context = context_name.clone();
                        w.action = Some(action.clone());
                        warnings.push(w);
                    }
                }
            }
        }
    }

    warnings
}

/// Detect duplicate keys within the same bindings block in a raw JSON string.
/// 1:1 with `checkDuplicateKeysInJson` (validate.ts:258-307).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn check_duplicate_keys_in_json(json_string: &str) -> Vec<KeybindingWarning> {
    static BLOCK_RE: OnceLock<Regex> = OnceLock::new();
    static CONTEXT_RE: OnceLock<Regex> = OnceLock::new();
    static KEY_RE: OnceLock<Regex> = OnceLock::new();

    let mut warnings = Vec::new();
    let block_re = BLOCK_RE.get_or_init(|| {
        Regex::new(r#""bindings"\s*:\s*\{([^{}]*(?:\{[^{}]*\}[^{}]*)*)\}"#).expect("static regex")
    });
    let context_re =
        CONTEXT_RE.get_or_init(|| Regex::new(r#""context"\s*:\s*"([^"]+)"[^{]*$"#).expect("re"));
    let key_re = KEY_RE.get_or_init(|| Regex::new(r#""([^"]+)"\s*:"#).expect("re"));

    for block_match in block_re.captures_iter(json_string) {
        let full = block_match.get(0).expect("group 0");
        let Some(block_content) = block_match.get(1).map(|m| m.as_str()) else {
            continue;
        };
        if block_content.is_empty() {
            continue;
        }

        // Find the context for this block by looking backwards.
        let text_before = &json_string[..full.start()];
        let context = context_re
            .captures(text_before)
            .and_then(|c| c.get(1))
            .map_or("unknown", |m| m.as_str())
            .to_string();

        // Find all keys within this bindings block.
        let mut keys_by_name: HashMap<String, usize> = HashMap::new();
        for key_match in key_re.captures_iter(block_content) {
            let Some(key) = key_match.get(1).map(|m| m.as_str()) else {
                continue;
            };
            let count = keys_by_name.entry(key.to_string()).or_insert(0);
            *count += 1;
            if *count == 2 {
                let mut w = KeybindingWarning::new(
                    KeybindingWarningType::Duplicate,
                    Severity::Warning,
                    format!("Duplicate key \"{key}\" in {context} bindings"),
                );
                w.key = Some(key.to_string());
                w.context = Some(context.clone());
                w.suggestion = Some(
                    "This key appears multiple times in the same context. JSON uses the last value, earlier values are ignored.".to_string(),
                );
                warnings.push(w);
            }
        }
    }

    warnings
}

/// Validate the user-config block array structure. 1:1 with `validateUserConfig`
/// (validate.ts:312-330) — but operating on already-extracted [`RawBlock`]s.
fn validate_user_config(user_blocks: &[RawBlock]) -> Vec<KeybindingWarning> {
    let mut warnings = Vec::new();
    for (i, b) in user_blocks.iter().enumerate() {
        warnings.extend(validate_block(b, i));
    }
    warnings
}

/// Check for duplicate bindings within the same context (parsed blocks).
/// 1:1 with `checkDuplicates` (validate.ts:336-368).
#[must_use]
pub fn check_duplicates(blocks: &[KeybindingBlock]) -> Vec<KeybindingWarning> {
    let mut warnings = Vec::new();
    // context -> (normalized key -> action string)
    let mut seen_by_context: HashMap<String, HashMap<String, String>> = HashMap::new();

    for block in blocks {
        let context_map = seen_by_context.entry(block.context.clone()).or_default();

        for (key, action) in &block.bindings {
            let normalized = normalize_key_for_comparison(key);
            let action_str = action.clone().unwrap_or_else(|| "null".to_string());
            if let Some(existing) = context_map.get(&normalized) {
                if existing != &action_str {
                    let mut w = KeybindingWarning::new(
                        KeybindingWarningType::Duplicate,
                        Severity::Warning,
                        format!("Duplicate binding \"{key}\" in {} context", block.context),
                    );
                    w.key = Some(key.clone());
                    w.context = Some(block.context.clone());
                    w.action = Some(
                        action
                            .clone()
                            .unwrap_or_else(|| "null (unbind)".to_string()),
                    );
                    w.suggestion = Some(format!(
                        "Previously bound to \"{existing}\". Only the last binding will be used."
                    ));
                    warnings.push(w);
                }
            }
            context_map.insert(normalized, action_str);
        }
    }
    warnings
}

/// Check user bindings against the reserved-shortcut table.
/// 1:1 with `checkReservedShortcuts` (validate.ts:373-399). `is_macos` is
/// injected so the macOS set is deterministic in tests.
#[must_use]
#[allow(clippy::assigning_clones)] // faithful per-field warning fill onto None fields
pub fn check_reserved_shortcuts(
    bindings: &[ParsedBinding],
    is_macos: bool,
) -> Vec<KeybindingWarning> {
    let mut warnings = Vec::new();
    let reserved = get_reserved_shortcuts_for(is_macos);

    for binding in bindings {
        let key_display = chord_to_string(&binding.chord);
        let normalized = normalize_key_for_comparison(&key_display);
        for res in &reserved {
            if normalize_key_for_comparison(res.key) == normalized {
                let mut w = KeybindingWarning::new(
                    KeybindingWarningType::Reserved,
                    res.severity,
                    format!("\"{key_display}\" may not work: {}", res.reason),
                );
                w.key = Some(key_display.clone());
                w.context = Some(binding.context.clone());
                w.action = binding.action.clone();
                warnings.push(w);
            }
        }
    }
    warnings
}

/// Parse raw user blocks into [`ParsedBinding`]s for reserved-shortcut checking.
/// 1:1 with `getUserBindingsForValidation` (validate.ts:405-420): note the chord
/// split here is on plain `' '` (single space), matching the TS verbatim.
fn get_user_bindings_for_validation(user_blocks: &[KeybindingBlock]) -> Vec<ParsedBinding> {
    let mut bindings = Vec::new();
    for block in user_blocks {
        for (key, action) in &block.bindings {
            let chord = key.split(' ').map(parse_keystroke).collect();
            bindings.push(ParsedBinding {
                chord,
                action: action.clone(),
                context: block.context.clone(),
            });
        }
    }
    bindings
}

/// Run all structural/semantic validations and return deduplicated warnings.
/// 1:1 with `validateBindings` (validate.ts:425-451).
///
/// `raw_blocks` are the loosely-typed blocks (for structural errors); `blocks`
/// are the successfully-extracted, well-formed [`KeybindingBlock`]s (the TS
/// `isKeybindingBlockArray(userBlocks)` branch — duplicate/reserved checks only
/// run when ALL blocks are well-formed).
#[must_use]
pub fn validate_bindings(
    raw_blocks: &[RawBlock],
    blocks: Option<&[KeybindingBlock]>,
    is_macos: bool,
) -> Vec<KeybindingWarning> {
    let mut warnings = Vec::new();

    warnings.extend(validate_user_config(raw_blocks));

    if let Some(blocks) = blocks {
        warnings.extend(check_duplicates(blocks));
        let user_bindings = get_user_bindings_for_validation(blocks);
        warnings.extend(check_reserved_shortcuts(&user_bindings, is_macos));
    }

    // Deduplicate by (type, key, context). 1:1 with validate.ts:443-450.
    let mut seen: HashSet<String> = HashSet::new();
    warnings
        .into_iter()
        .filter(|w| {
            let key = format!(
                "{:?}:{}:{}",
                w.r#type,
                w.key.as_deref().unwrap_or(""),
                w.context.as_deref().unwrap_or("")
            );
            seen.insert(key)
        })
        .collect()
}

/// Format a single warning for display. 1:1 with `formatWarning`
/// (validate.ts:456-465).
#[must_use]
pub fn format_warning(warning: &KeybindingWarning) -> String {
    let icon = if warning.severity == Severity::Error {
        "✗"
    } else {
        "⚠"
    };
    let sev = if warning.severity == Severity::Error {
        "error"
    } else {
        "warning"
    };
    let mut msg = format!("{icon} Keybinding {sev}: {}", warning.message);
    if let Some(s) = &warning.suggestion {
        msg.push_str(&format!("\n  {s}"));
    }
    msg
}

/// Format multiple warnings (errors then warnings). 1:1 with `formatWarnings`
/// (validate.ts:470-498).
#[must_use]
pub fn format_warnings(warnings: &[KeybindingWarning]) -> String {
    if warnings.is_empty() {
        return String::new();
    }
    let errors: Vec<&KeybindingWarning> = warnings
        .iter()
        .filter(|w| w.severity == Severity::Error)
        .collect();
    let warns: Vec<&KeybindingWarning> = warnings
        .iter()
        .filter(|w| w.severity == Severity::Warning)
        .collect();

    let mut lines: Vec<String> = Vec::new();
    if !errors.is_empty() {
        lines.push(format!(
            "Found {} keybinding {}:",
            errors.len(),
            plural(errors.len(), "error")
        ));
        for e in errors {
            lines.push(format_warning(e));
        }
    }
    if !warns.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!(
            "Found {} keybinding {}:",
            warns.len(),
            plural(warns.len(), "warning")
        ));
        for w in warns {
            lines.push(format_warning(w));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn block(context: &str, pairs: &[(&str, Option<&str>)]) -> KeybindingBlock {
        let mut bindings = IndexMap::new();
        for (k, v) in pairs {
            bindings.insert((*k).to_string(), v.map(str::to_string));
        }
        KeybindingBlock {
            context: context.to_string(),
            bindings,
        }
    }

    #[test]
    fn unknown_context_is_invalid_context_error() {
        let raw = RawBlock {
            context: Some("Bogus".to_string()),
            context_missing: false,
            bindings: vec![RawBinding {
                key: "ctrl+l".to_string(),
                action: Some("app:redraw".to_string()),
                non_string: false,
            }],
            bindings_missing: false,
        };
        let w = validate_bindings(&[raw], None, false);
        assert!(w
            .iter()
            .any(|x| x.r#type == KeybindingWarningType::InvalidContext
                && x.severity == Severity::Error));
    }

    #[test]
    fn non_string_action_is_invalid_action_error() {
        let raw = RawBlock {
            context: Some("Global".to_string()),
            context_missing: false,
            bindings: vec![RawBinding {
                key: "ctrl+l".to_string(),
                action: None,
                non_string: true,
            }],
            bindings_missing: false,
        };
        let w = validate_bindings(&[raw], None, false);
        assert!(w
            .iter()
            .any(|x| x.r#type == KeybindingWarningType::InvalidAction
                && x.severity == Severity::Error
                && x.message.contains("must be a string or null")));
    }

    #[test]
    fn command_binding_outside_chat_warns() {
        let raw = RawBlock {
            context: Some("Global".to_string()),
            context_missing: false,
            bindings: vec![RawBinding {
                key: "ctrl+h".to_string(),
                action: Some("command:help".to_string()),
                non_string: false,
            }],
            bindings_missing: false,
        };
        let w = validate_bindings(&[raw], None, false);
        assert!(w.iter().any(|x| x.severity == Severity::Warning
            && x.message.contains("must be in \"Chat\" context")));
    }

    #[test]
    fn duplicate_keys_in_raw_json_warn_on_second() {
        let json = r#"{ "bindings": [ { "context": "Global", "bindings": { "ctrl+l": "app:redraw", "ctrl+l": "app:toggleTodos" } } ] }"#;
        let w = check_duplicate_keys_in_json(json);
        assert_eq!(
            w.iter()
                .filter(|x| x.r#type == KeybindingWarningType::Duplicate)
                .count(),
            1
        );
        assert_eq!(w[0].key.as_deref(), Some("ctrl+l"));
        assert_eq!(w[0].context.as_deref(), Some("Global"));
    }

    #[test]
    fn reserved_ctrl_c_flagged_as_error() {
        let b = block("Global", &[("ctrl+c", Some("app:toggleTodos"))]);
        let parsed = super::super::parser::parse_bindings(std::slice::from_ref(&b));
        let w = check_reserved_shortcuts(&parsed, false);
        assert!(w
            .iter()
            .any(|x| x.severity == Severity::Error && x.key.as_deref() == Some("ctrl+c")));
    }
}
