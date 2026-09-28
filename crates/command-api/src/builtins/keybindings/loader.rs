//! User keybinding loader — a 1:1 port of the load path in
//! `claude-code/src/keybindings/loadUserBindings.ts`.
//!
//! ## What is ported
//!
//! `loadKeybindings` / `loadKeybindingsSyncWithWarnings`: gate
//! (`isKeybindingCustomizationEnabled`) → read `~/.lingxi/keybindings.json` →
//! require the `{ "bindings": [...] }` object wrapper (else `parse_error`) →
//! validate block structure → merge `[...defaults, ...userParsed]` (user
//! appended so last-wins overrides) → raw-JSON duplicate-key check +
//! `validateBindings` → ENOENT/other-error → defaults.
//!
//! ## Accepted divergences (documented residual)
//!
//! - **Gate.** claude-code gates on the `tengu_keybinding_customization_release`
//!   `GrowthBook` flag (off for external users → defaults only). There is no
//!   `GrowthBook` here; the gate is an injected `bool` defaulting to `false`,
//!   matching the existing `/keybindings` handler. With the gate off (default)
//!   this returns `default_bindings()` parsed — byte-identical to the hardcoded
//!   TUI behavior.
//! - **Hot-reload watcher.** `initializeKeybindingWatcher` (chokidar, 500ms
//!   awaitWriteFinish, reset-on-unlink) is NOT ported: the iocraft render loop
//!   has no config-watch seam, and the loader re-reads per session start, which
//!   matches the canonical no-watcher external path (gate off by default). This
//!   is the load logic only — a pure function with the path + gate injected.
//! - **Telemetry.** `logCustomBindingsLoadedOncePerDay` (once-per-day analytics
//!   event) is omitted; it has no behavioral effect on resolution.

use super::default_bindings::default_bindings;
use super::parser::parse_bindings;
use super::types::{KeybindingBlock, ParsedBinding};
use super::validate::{
    check_duplicate_keys_in_json, validate_bindings, KeybindingWarning, KeybindingWarningType,
    RawBinding, RawBlock,
};
use indexmap::IndexMap;
use std::path::Path;

/// Result of loading keybindings: merged bindings + any validation warnings.
/// 1:1 with the TS `KeybindingsLoadResult`.
#[derive(Debug, Clone)]
pub struct KeybindingsLoadResult {
    /// The merged (default + user) flattened bindings, ready for the resolver.
    pub bindings: Vec<ParsedBinding>,
    /// Validation warnings/errors. Never fatal — bindings are always returned.
    pub warnings: Vec<KeybindingWarning>,
}

/// The parsed default bindings (the TS `getDefaultParsedBindings`).
fn default_parsed_bindings() -> Vec<ParsedBinding> {
    parse_bindings(&default_bindings())
}

fn parse_error(message: &str, suggestion: &str) -> KeybindingWarning {
    KeybindingWarning {
        r#type: KeybindingWarningType::ParseError,
        severity: super::reserved::Severity::Error,
        message: message.to_string(),
        key: None,
        context: None,
        action: None,
        suggestion: Some(suggestion.to_string()),
    }
}

/// Load and merge keybindings from a user config file.
///
/// 1:1 with `loadKeybindings` (loadUserBindings.ts:133-237): gate → read →
/// wrapper-validate → struct-validate → merge → validate. `enabled` is the
/// injected gate (the TS `isKeybindingCustomizationEnabled()`); `path` is the
/// injected file path (the TS `getKeybindingsPath()`); `is_macos` selects the
/// reserved-shortcut platform set in validation.
///
/// For external users (`enabled == false`) this always returns the default
/// bindings only — no file I/O — exactly like the TS early return.
#[must_use]
pub fn load_keybindings(enabled: bool, path: &Path, is_macos: bool) -> KeybindingsLoadResult {
    let default = default_parsed_bindings();

    if !enabled {
        return KeybindingsLoadResult {
            bindings: default,
            warnings: vec![],
        };
    }

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // File doesn't exist — defaults (user can run /keybindings).
            return KeybindingsLoadResult {
                bindings: default,
                warnings: vec![],
            };
        }
        Err(e) => {
            return KeybindingsLoadResult {
                bindings: default,
                warnings: vec![KeybindingWarning {
                    r#type: KeybindingWarningType::ParseError,
                    severity: super::reserved::Severity::Error,
                    message: format!("Failed to parse keybindings.json: {e}"),
                    key: None,
                    context: None,
                    action: None,
                    suggestion: None,
                }],
            };
        }
    };

    let parsed: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            // The TS `jsonParse` throwing lands in the catch → defaults + a
            // "Failed to parse" parse_error (loadUserBindings.ts:222-235).
            return KeybindingsLoadResult {
                bindings: default,
                warnings: vec![KeybindingWarning {
                    r#type: KeybindingWarningType::ParseError,
                    severity: super::reserved::Severity::Error,
                    message: format!("Failed to parse keybindings.json: {e}"),
                    key: None,
                    context: None,
                    action: None,
                    suggestion: None,
                }],
            };
        }
    };

    // Extract bindings array from the object wrapper: { "bindings": [...] }.
    // 1:1 with loadUserBindings.ts:148-167.
    let Some(bindings_val) = parsed.as_object().and_then(|o| o.get("bindings")) else {
        return KeybindingsLoadResult {
            bindings: default,
            warnings: vec![parse_error(
                "keybindings.json must have a \"bindings\" array",
                "Use format: { \"bindings\": [ ... ] }",
            )],
        };
    };

    // Validate structure — must be an array of valid keybinding blocks.
    // 1:1 with loadUserBindings.ts:170-189 (isKeybindingBlockArray).
    let Some(arr) = bindings_val.as_array() else {
        return KeybindingsLoadResult {
            bindings: default,
            warnings: vec![parse_error(
                "\"bindings\" must be an array",
                "Set \"bindings\" to an array of keybinding blocks",
            )],
        };
    };
    if !arr.iter().all(is_keybinding_block) {
        return KeybindingsLoadResult {
            bindings: default,
            warnings: vec![parse_error(
                "keybindings.json contains invalid block structure",
                "Each block must have \"context\" (string) and \"bindings\" (object)",
            )],
        };
    }

    // All blocks are well-formed: extract typed blocks + raw blocks.
    let typed_blocks: Vec<KeybindingBlock> = arr.iter().map(extract_typed_block).collect();
    let raw_blocks: Vec<RawBlock> = arr.iter().map(extract_raw_block).collect();

    let user_parsed = parse_bindings(&typed_blocks);

    // User bindings come AFTER defaults, so they override (last-wins).
    let mut merged = default;
    merged.extend(user_parsed);

    // Validation: raw-JSON duplicate keys first, then structural/semantic.
    let mut warnings = check_duplicate_keys_in_json(&content);
    warnings.extend(validate_bindings(
        &raw_blocks,
        Some(&typed_blocks),
        is_macos,
    ));

    KeybindingsLoadResult {
        bindings: merged,
        warnings,
    }
}

/// Convenience: load against the real `~/.lingxi/keybindings.json` path on the
/// host platform. Mirrors `getKeybindingsPath()` via the existing handler's
/// [`super::keybindings_path`].
#[must_use]
pub fn load_keybindings_default_path(enabled: bool) -> KeybindingsLoadResult {
    load_keybindings(
        enabled,
        &super::keybindings_path(),
        cfg!(target_os = "macos"),
    )
}

/// 1:1 with the `isKeybindingBlock` type guard (loadUserBindings.ts:95-103):
/// an object with a string `context` and an object `bindings`.
fn is_keybinding_block(v: &serde_json::Value) -> bool {
    let Some(o) = v.as_object() else { return false };
    o.get("context").is_some_and(serde_json::Value::is_string)
        && o.get("bindings").is_some_and(serde_json::Value::is_object)
}

/// Extract a well-formed [`KeybindingBlock`] from a value already known to pass
/// [`is_keybinding_block`]. A non-string action value becomes `None` (unbind),
/// matching how `parseBindings` would treat a `null`; the non-string-error path
/// is surfaced separately by validation over the raw block.
fn extract_typed_block(v: &serde_json::Value) -> KeybindingBlock {
    let o = v.as_object().expect("is_keybinding_block");
    let context = o
        .get("context")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut bindings = IndexMap::new();
    if let Some(map) = o.get("bindings").and_then(serde_json::Value::as_object) {
        for (k, val) in map {
            let action = match val {
                serde_json::Value::String(s) => Some(s.clone()),
                // null and non-string both become "no concrete action" for the
                // typed/merge path; the raw block carries the non-string flag for
                // the InvalidAction warning.
                _ => None,
            };
            bindings.insert(k.clone(), action);
        }
    }
    KeybindingBlock { context, bindings }
}

/// Extract a loosely-typed [`RawBlock`] for structural validation.
fn extract_raw_block(v: &serde_json::Value) -> RawBlock {
    let o = v.as_object();
    let context = o
        .and_then(|o| o.get("context"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let context_missing = o
        .and_then(|o| o.get("context"))
        .is_none_or(|c| !c.is_string());
    let bindings_obj = o
        .and_then(|o| o.get("bindings"))
        .and_then(serde_json::Value::as_object);
    let bindings_missing = bindings_obj.is_none();
    let bindings = bindings_obj
        .map(|map| {
            map.iter()
                .map(|(k, val)| {
                    let (action, non_string) = match val {
                        serde_json::Value::String(s) => (Some(s.clone()), false),
                        serde_json::Value::Null => (None, false),
                        _ => (None, true),
                    };
                    RawBinding {
                        key: k.clone(),
                        action,
                        non_string,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    RawBlock {
        context,
        context_missing,
        bindings,
        bindings_missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(tag: &str, content: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let p = std::env::temp_dir().join(format!(
            "lingxi-kb-loader-{tag}-{}-{nanos}.json",
            std::process::id()
        ));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    fn action_for(res: &KeybindingsLoadResult, chord: &str, ctx: &str) -> Option<String> {
        use super::super::parser::parse_chord;
        let want = parse_chord(chord);
        res.bindings
            .iter()
            .rev()
            .find(|b| b.context == ctx && b.chord == want)
            .and_then(|b| b.action.clone())
    }

    #[test]
    fn no_file_returns_defaults_no_warnings() {
        let missing = std::env::temp_dir().join("lingxi-kb-does-not-exist-xyz.json");
        let res = load_keybindings(true, &missing, false);
        assert!(res.warnings.is_empty());
        // ctrl+l → app:redraw default is present.
        assert_eq!(
            action_for(&res, "ctrl+l", "Global"),
            Some("app:redraw".to_string())
        );
    }

    #[test]
    fn disabled_gate_returns_defaults() {
        // Even with a real file, the gate off returns defaults only.
        let p = tmp(
            "disabled",
            r#"{ "bindings": [ { "context": "Global", "bindings": { "ctrl+l": "app:toggleTodos" } } ] }"#,
        );
        let res = load_keybindings(false, &p, false);
        assert!(res.warnings.is_empty());
        assert_eq!(
            action_for(&res, "ctrl+l", "Global"),
            Some("app:redraw".to_string())
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn missing_bindings_wrapper_is_parse_error() {
        let p = tmp("nowrap", r#"{ "foo": 1 }"#);
        let res = load_keybindings(true, &p, false);
        assert!(res
            .warnings
            .iter()
            .any(|w| w.r#type == KeybindingWarningType::ParseError));
        // Still falls back to defaults.
        assert_eq!(
            action_for(&res, "ctrl+l", "Global"),
            Some("app:redraw".to_string())
        );
        let _ = std::fs::remove_file(&p);
    }
}
