//! Reserved-shortcut tables + key normalization — a 1:1 port of
//! `claude-code/src/keybindings/reservedShortcuts.ts`.

/// Severity of a reserved-shortcut conflict.
/// 1:1 with the TS `ReservedShortcut.severity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A hard error (e.g. non-rebindable, OS-intercepted).
    Error,
    /// A soft warning (e.g. terminal-intercepted, may not reach the app).
    Warning,
}

/// A shortcut typically intercepted by the OS/terminal/shell, or hardcoded.
/// 1:1 with the TS `ReservedShortcut`.
#[derive(Debug, Clone, Copy)]
pub struct ReservedShortcut {
    /// The key chord string (e.g. `"ctrl+c"`).
    pub key: &'static str,
    /// Why it's reserved.
    pub reason: &'static str,
    /// Error vs. warning.
    pub severity: Severity,
}

/// Shortcuts that cannot be rebound — hardcoded in Claude Code.
/// 1:1 with `NON_REBINDABLE` (reservedShortcuts.ts:16-33). The `capslock`
/// entry is present in the v2.1.185 binary's reserved table (offset
/// 157646642) but absent from the older leaked `reservedShortcuts.ts`;
/// the binary is canonical.
pub const NON_REBINDABLE: &[ReservedShortcut] = &[
    ReservedShortcut {
        key: "ctrl+c",
        reason: "Cannot be rebound - used for interrupt/exit (hardcoded)",
        severity: Severity::Error,
    },
    ReservedShortcut {
        key: "ctrl+d",
        reason: "Cannot be rebound - used for exit (hardcoded)",
        severity: Severity::Error,
    },
    ReservedShortcut {
        key: "ctrl+m",
        reason: "Cannot be rebound - identical to Enter in terminals (both send CR)",
        severity: Severity::Error,
    },
    ReservedShortcut {
        key: "capslock",
        reason: "Caps Lock is not delivered to terminal applications",
        severity: Severity::Error,
    },
];

/// Terminal control shortcuts intercepted by the terminal/OS.
/// 1:1 with `TERMINAL_RESERVED` (reservedShortcuts.ts:43-54).
pub const TERMINAL_RESERVED: &[ReservedShortcut] = &[
    ReservedShortcut {
        key: "ctrl+z",
        reason: "Unix process suspend (SIGTSTP)",
        severity: Severity::Warning,
    },
    ReservedShortcut {
        key: "ctrl+\\",
        reason: "Terminal quit signal (SIGQUIT)",
        severity: Severity::Error,
    },
];

/// macOS-specific shortcuts the OS intercepts.
/// 1:1 with `MACOS_RESERVED` (reservedShortcuts.ts:59-67).
pub const MACOS_RESERVED: &[ReservedShortcut] = &[
    ReservedShortcut {
        key: "cmd+c",
        reason: "macOS system copy",
        severity: Severity::Error,
    },
    ReservedShortcut {
        key: "cmd+v",
        reason: "macOS system paste",
        severity: Severity::Error,
    },
    ReservedShortcut {
        key: "cmd+x",
        reason: "macOS system cut",
        severity: Severity::Error,
    },
    ReservedShortcut {
        key: "cmd+q",
        reason: "macOS quit application",
        severity: Severity::Error,
    },
    ReservedShortcut {
        key: "cmd+w",
        reason: "macOS close window/tab",
        severity: Severity::Error,
    },
    ReservedShortcut {
        key: "cmd+tab",
        reason: "macOS app switcher",
        severity: Severity::Error,
    },
    ReservedShortcut {
        key: "cmd+space",
        reason: "macOS Spotlight",
        severity: Severity::Error,
    },
];

/// Get all reserved shortcuts for the given platform.
/// 1:1 with `getReservedShortcuts` (reservedShortcuts.ts:73-83): non-rebindable
/// first, then terminal-reserved, plus the macOS set on macOS.
///
/// `is_macos` is injected (the TS `getPlatform() === 'macos'`) so validation is
/// deterministic in tests regardless of the host OS.
#[must_use]
pub fn get_reserved_shortcuts_for(is_macos: bool) -> Vec<ReservedShortcut> {
    let mut reserved: Vec<ReservedShortcut> = Vec::new();
    reserved.extend_from_slice(NON_REBINDABLE);
    reserved.extend_from_slice(TERMINAL_RESERVED);
    if is_macos {
        reserved.extend_from_slice(MACOS_RESERVED);
    }
    reserved
}

/// [`get_reserved_shortcuts_for`] with the host platform (the TS default path).
#[must_use]
pub fn get_reserved_shortcuts() -> Vec<ReservedShortcut> {
    get_reserved_shortcuts_for(cfg!(target_os = "macos"))
}

/// Normalize a key string for comparison (lowercase, per-step sorted modifiers).
/// 1:1 with `normalizeKeyForComparison` (reservedShortcuts.ts:91-93): chords are
/// normalized per space-separated step (splitting on `+` first would mangle a
/// chord into its last key).
#[must_use]
pub fn normalize_key_for_comparison(key: &str) -> String {
    // `split_whitespace` subsumes the TS `key.trim().split(/\s+/)`.
    key.split_whitespace()
        .map(normalize_step)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalize a single chord step (sorted modifiers + main key).
/// 1:1 with `normalizeStep` (reservedShortcuts.ts:95-127).
fn normalize_step(step: &str) -> String {
    let mut modifiers: Vec<String> = Vec::new();
    let mut main_key = String::new();

    for part in step.split('+') {
        let lower = part.trim().to_lowercase();
        if matches!(
            lower.as_str(),
            "ctrl" | "control" | "alt" | "opt" | "option" | "meta" | "cmd" | "command" | "shift"
        ) {
            // Normalize modifier names (match the TS branch order/results).
            if lower == "control" {
                modifiers.push("ctrl".to_string());
            } else if lower == "option" || lower == "opt" {
                modifiers.push("alt".to_string());
            } else if lower == "command" || lower == "cmd" {
                modifiers.push("cmd".to_string());
            } else {
                modifiers.push(lower);
            }
        } else {
            main_key = lower;
        }
    }

    modifiers.sort();
    modifiers.push(main_key);
    modifiers.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_sorts_modifiers_per_step() {
        // shift+ctrl+a → ctrl+shift+a (sorted), and a chord normalizes per step.
        assert_eq!(normalize_key_for_comparison("shift+ctrl+a"), "ctrl+shift+a");
        assert_eq!(
            normalize_key_for_comparison("ctrl+x ctrl+b"),
            "ctrl+x ctrl+b"
        );
    }

    #[test]
    fn normalize_aliases_modifiers() {
        assert_eq!(normalize_key_for_comparison("command+c"), "cmd+c");
        assert_eq!(normalize_key_for_comparison("option+k"), "alt+k");
        assert_eq!(normalize_key_for_comparison("control+l"), "ctrl+l");
    }

    #[test]
    fn macos_set_only_on_macos() {
        let mac = get_reserved_shortcuts_for(true);
        let other = get_reserved_shortcuts_for(false);
        assert!(mac.iter().any(|r| r.key == "cmd+c"));
        assert!(!other.iter().any(|r| r.key == "cmd+c"));
    }
}
