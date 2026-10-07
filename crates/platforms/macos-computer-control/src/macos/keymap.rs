//! Name → `enigo::Key` mapping for the `key`/`hold_key` actions.
//!
//! Parity target: claude-code's key names are xdotool-style, `+`-joined,
//! case-insensitive chords (`"cmd+shift+a"`, `"ctrl+c"`, `"return"`,
//! `"escape"`). A single unmapped character falls back to `Key::Unicode`, so
//! any printable character works as a key name without an explicit table
//! entry.

use enigo::Key;

/// One parsed chord: modifiers to press first (in order), then the final key
/// to click, then modifiers released in reverse order. A bare key (no `+`)
/// has an empty modifier list.
pub struct Chord {
    pub modifiers: Vec<Key>,
    pub main: Key,
}

/// Parse a single key name (one chord segment) into an `enigo::Key`. Named
/// keys are matched case-insensitively; anything else must be exactly one
/// character and becomes `Key::Unicode`.
#[must_use]
pub fn parse_key_name(name: &str) -> Option<Key> {
    let lower = lingxi_core::host::computer_control::canonical_computer_key(name);
    let named = match lower.as_str() {
        "return" | "enter" => Key::Return,
        "escape" | "esc" => Key::Escape,
        "tab" => Key::Tab,
        "space" | "spacebar" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "up" | "uparrow" => Key::UpArrow,
        "down" | "downarrow" => Key::DownArrow,
        "left" | "leftarrow" => Key::LeftArrow,
        "right" | "rightarrow" => Key::RightArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "page_up" => Key::PageUp,
        "pagedown" | "page_down" => Key::PageDown,
        "capslock" | "caps_lock" => Key::CapsLock,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        "shift" => Key::Shift,
        "ctrl" | "control" => Key::Control,
        "alt" | "option" | "opt" => Key::Alt,
        "cmd" | "command" | "meta" | "super" | "win" | "windows" => Key::Meta,
        _ => {
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None; // not a single char and not a known name
            }
            return Some(Key::Unicode(c));
        }
    };
    Some(named)
}

/// Parse a full `+`-joined chord (e.g. `"ctrl+shift+a"`). Empty segments
/// (leading/trailing/doubled `+`) are dropped. Returns `None` if the chord is
/// empty or any segment fails to parse.
#[must_use]
pub fn parse_chord(spec: &str) -> Option<Chord> {
    let mut parts: Vec<&str> = spec.split('+').filter(|p| !p.is_empty()).collect();
    let last = parts.pop()?;
    let main = parse_key_name(last)?;
    let mut modifiers = Vec::with_capacity(parts.len());
    for p in parts {
        modifiers.push(parse_key_name(p)?);
    }
    Some(Chord { modifiers, main })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_named_key() {
        assert!(matches!(parse_key_name("Return"), Some(Key::Return)));
        assert!(matches!(parse_key_name("ESCAPE"), Some(Key::Escape)));
        assert!(matches!(parse_key_name("esc"), Some(Key::Escape)));
    }

    #[test]
    fn parses_single_char_as_unicode() {
        assert!(matches!(parse_key_name("a"), Some(Key::Unicode('a'))));
        assert!(matches!(parse_key_name("Z"), Some(Key::Unicode('Z'))));
    }

    #[test]
    fn rejects_unknown_multi_char() {
        assert!(parse_key_name("notakey").is_none());
    }

    #[test]
    fn parses_modifier_chord_in_order() {
        let chord = parse_chord("cmd+shift+a").expect("chord");
        assert!(matches!(chord.modifiers[0], Key::Meta));
        assert!(matches!(chord.modifiers[1], Key::Shift));
        assert!(matches!(chord.main, Key::Unicode('a')));
    }

    #[test]
    fn bare_key_has_no_modifiers() {
        let chord = parse_chord("return").expect("chord");
        assert!(chord.modifiers.is_empty());
        assert!(matches!(chord.main, Key::Return));
    }
}
