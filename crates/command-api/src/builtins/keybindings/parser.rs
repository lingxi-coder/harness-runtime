//! Keystroke / chord parsing + display — a 1:1 port of
//! `claude-code/src/keybindings/parser.ts`.

use super::types::{Chord, KeybindingBlock, ParsedBinding, ParsedKeystroke};

/// Parse a keystroke string like `"ctrl+shift+k"` into a [`ParsedKeystroke`].
///
/// 1:1 with `parseKeystroke` (parser.ts:13-75): modifier aliases
/// (`ctrl`/`control`, `alt`/`opt`/`option`, `meta`, `cmd`/`command`/`super`/`win`)
/// and key aliases (`esc→escape`, `return→enter`, `space→' '`, arrow glyphs).
#[must_use]
pub fn parse_keystroke(input: &str) -> ParsedKeystroke {
    let mut ks = ParsedKeystroke::default();
    for part in input.split('+') {
        let lower = part.to_lowercase();
        match lower.as_str() {
            "ctrl" | "control" => ks.ctrl = true,
            "alt" | "opt" | "option" => ks.alt = true,
            "shift" => ks.shift = true,
            "meta" => ks.meta = true,
            "cmd" | "command" | "super" | "win" => ks.super_ = true,
            "esc" => ks.key = "escape".to_string(),
            "return" => ks.key = "enter".to_string(),
            "space" => ks.key = " ".to_string(),
            "↑" => ks.key = "up".to_string(),
            "↓" => ks.key = "down".to_string(),
            "←" => ks.key = "left".to_string(),
            "→" => ks.key = "right".to_string(),
            _ => ks.key = lower,
        }
    }
    ks
}

/// Parse a chord string like `"ctrl+k ctrl+s"` into a [`Chord`].
///
/// 1:1 with `parseChord` (parser.ts:80-84): a lone space `" "` IS the space
/// key binding (not a separator); otherwise split on whitespace runs.
#[must_use]
pub fn parse_chord(input: &str) -> Chord {
    if input == " " {
        return vec![parse_keystroke("space")];
    }
    // `split_whitespace` already skips leading/trailing whitespace, so it
    // subsumes the TS `input.trim().split(/\s+/)` (faithful: same token set).
    input.split_whitespace().map(parse_keystroke).collect()
}

/// Convert a [`ParsedKeystroke`] to its canonical string for display.
/// 1:1 with `keystrokeToString` (parser.ts:89-100).
#[must_use]
pub fn keystroke_to_string(ks: &ParsedKeystroke) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if ks.ctrl {
        parts.push("ctrl");
    }
    if ks.alt {
        parts.push("alt");
    }
    if ks.shift {
        parts.push("shift");
    }
    if ks.meta {
        parts.push("meta");
    }
    if ks.super_ {
        parts.push("cmd");
    }
    let display_key = key_to_display_name(&ks.key);
    let mut owned: Vec<String> = parts.into_iter().map(str::to_string).collect();
    owned.push(display_key);
    owned.join("+")
}

/// Map internal key names to human-readable display names.
/// 1:1 with `keyToDisplayName` (parser.ts:105-138).
#[must_use]
pub fn key_to_display_name(key: &str) -> String {
    match key {
        "escape" => "Esc",
        " " => "Space",
        "tab" => "tab",
        "enter" => "Enter",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "up" => "↑",
        "down" => "↓",
        "left" => "←",
        "right" => "→",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "home" => "Home",
        "end" => "End",
        other => other,
    }
    .to_string()
}

/// Convert a [`Chord`] to its canonical display string.
/// 1:1 with `chordToString` (parser.ts:143-145).
#[must_use]
pub fn chord_to_string(chord: &Chord) -> String {
    chord
        .iter()
        .map(keystroke_to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Display platform — a subset of Platform that matters for display.
/// WSL and unknown are treated as linux. 1:1 with the TS `DisplayPlatform`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayPlatform {
    /// macOS — uses `opt` for alt and `cmd` for super.
    Macos,
    /// Windows.
    Windows,
    /// Linux (and WSL / unknown, per the TS comment).
    Linux,
}

/// Convert a [`ParsedKeystroke`] to a platform-appropriate display string.
/// 1:1 with `keystrokeToDisplayString` (parser.ts:157-176): `opt` for alt on
/// macOS (`alt` elsewhere); alt/meta collapse; `cmd` (macOS) vs `super`.
#[must_use]
pub fn keystroke_to_display_string(ks: &ParsedKeystroke, platform: DisplayPlatform) -> String {
    let mut parts: Vec<String> = Vec::new();
    if ks.ctrl {
        parts.push("ctrl".to_string());
    }
    // Alt/meta are equivalent in terminals — show the platform-appropriate name.
    if ks.alt || ks.meta {
        parts.push(if platform == DisplayPlatform::Macos {
            "opt".to_string()
        } else {
            "alt".to_string()
        });
    }
    if ks.shift {
        parts.push("shift".to_string());
    }
    if ks.super_ {
        parts.push(if platform == DisplayPlatform::Macos {
            "cmd".to_string()
        } else {
            "super".to_string()
        });
    }
    parts.push(key_to_display_name(&ks.key));
    parts.join("+")
}

/// Convert a [`Chord`] to a platform-appropriate display string.
/// 1:1 with `chordToDisplayString` (parser.ts:181-186).
#[must_use]
pub fn chord_to_display_string(chord: &Chord, platform: DisplayPlatform) -> String {
    chord
        .iter()
        .map(|ks| keystroke_to_display_string(ks, platform))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse keybinding blocks into a flat list of [`ParsedBinding`]s.
/// 1:1 with `parseBindings` (parser.ts:191-203): iterates each block's bindings
/// in insertion order.
#[must_use]
pub fn parse_bindings(blocks: &[KeybindingBlock]) -> Vec<ParsedBinding> {
    let mut bindings = Vec::new();
    for block in blocks {
        for (key, action) in &block.bindings {
            bindings.push(ParsedBinding {
                chord: parse_chord(key),
                action: action.clone(),
                context: block.context.clone(),
            });
        }
    }
    bindings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_modifier_aliases() {
        let ks = parse_keystroke("control+opt+cmd+k");
        assert!(ks.ctrl && ks.alt && ks.super_);
        assert_eq!(ks.key, "k");
    }

    #[test]
    fn esc_return_space_aliases() {
        assert_eq!(parse_keystroke("esc").key, "escape");
        assert_eq!(parse_keystroke("return").key, "enter");
        assert_eq!(parse_keystroke("space").key, " ");
    }

    #[test]
    fn lone_space_is_space_key() {
        let c = parse_chord(" ");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].key, " ");
    }

    #[test]
    fn chord_splits_on_whitespace() {
        let c = parse_chord("ctrl+x ctrl+k");
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].key, "x");
        assert_eq!(c[1].key, "k");
        assert!(c[0].ctrl && c[1].ctrl);
    }

    #[test]
    fn display_string_macos_opt_cmd() {
        let ks = parse_keystroke("alt+cmd+up");
        assert_eq!(
            keystroke_to_display_string(&ks, DisplayPlatform::Macos),
            "opt+cmd+↑"
        );
        assert_eq!(
            keystroke_to_display_string(&ks, DisplayPlatform::Linux),
            "alt+super+↑"
        );
    }
}
