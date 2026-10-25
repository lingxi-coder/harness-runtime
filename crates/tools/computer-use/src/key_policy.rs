//! Key capability classifier, verified against the 2.1.310 native helper.
fn parse(chord: &str) -> (Vec<String>, Vec<String>, Vec<(String, bool)>) {
    let mut ordered = Vec::new();
    for part in chord.to_lowercase().split('+') {
        let mut token = part.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
        if token.is_empty() {
            if part.len() == 1 {
                token = part;
            } else {
                continue;
            }
        }
        let modifier = match token {
            "meta" | "super" | "command" | "cmd" | "windows" | "win" => Some("meta"),
            "ctrl" | "control" | "lctrl" | "lcontrol" | "rctrl" | "rcontrol" => Some("ctrl"),
            "shift" | "lshift" | "rshift" => Some("shift"),
            "alt" | "option" => Some("alt"),
            _ => None,
        };
        let name = modifier.unwrap_or(match token {
            "esc" | "\u{1b}" => "escape",
            "del" | "\u{7f}" => "delete",
            " " => "space",
            "\t" => "tab",
            _ => token,
        });
        ordered.push((name.to_owned(), modifier.is_some()));
    }
    let mods = sort_mods(
        ordered
            .iter()
            .filter(|(_, is_mod)| *is_mod)
            .map(|(name, _)| name.clone())
            .collect(),
    );
    let keys = ordered
        .iter()
        .filter(|(_, is_mod)| !is_mod)
        .map(|(name, _)| name.clone())
        .collect();
    (mods, keys, ordered)
}
fn sort_mods(mut mods: Vec<String>) -> Vec<String> {
    mods.sort_by_key(|name| {
        ["ctrl", "alt", "shift", "meta"]
            .iter()
            .position(|item| item == name)
            .unwrap_or(4)
    });
    mods.dedup();
    mods
}
pub fn clipboard_flags(chord: &str) -> (bool, bool) {
    let (mods, keys, _) = parse(chord);
    let command = mods.iter().any(|m| m == "ctrl" || m == "meta");
    let shift = mods.iter().any(|m| m == "shift");
    let ctrl = mods.iter().any(|m| m == "ctrl");
    (
        keys.iter()
            .any(|key| command && key == "v" || shift && key == "insert"),
        keys.iter().any(|key| {
            command && matches!(key.as_str(), "c" | "x")
                || ctrl && key == "insert"
                || shift && key == "delete"
        }),
    )
}
pub fn system_shortcut(chord: &str, darwin: bool) -> bool {
    let mac = [
        "meta+q",
        "shift+meta+q",
        "alt+shift+meta+q",
        "alt+meta+escape",
        "meta+tab",
        "shift+meta+tab",
        "meta+space",
        "ctrl+meta+q",
        "ctrl+f2",
        "ctrl+f3",
        "ctrl+f8",
        "ctrl+up",
        "ctrl+down",
    ];
    let win = [
        "ctrl+alt+delete",
        "alt+f4",
        "alt+tab",
        "alt+shift+tab",
        "ctrl+alt+tab",
        "ctrl+alt+shift+tab",
        "meta+l",
        "meta+d",
        "meta+r",
        "meta+e",
        "meta+s",
        "meta+q",
        "ctrl+escape",
        "meta+i",
        "meta+u",
        "meta+x",
    ];
    let patterns: &[&str] = if darwin { &mac } else { &win };
    let (mods, keys, ordered) = parse(chord);
    let matches = |mods: &[String], key: Option<&str>| {
        let mut parts = mods.to_vec();
        if let Some(key) = key {
            parts.push(key.to_owned());
        }
        patterns.contains(&parts.join("+").as_str())
    };
    if keys.is_empty() {
        return matches(&mods, None);
    }
    if keys.iter().any(|key| matches(&mods, Some(key))) {
        return true;
    }
    let mut prefix = Vec::new();
    for (name, is_mod) in ordered {
        if is_mod {
            prefix.push(name);
        } else if matches(&sort_mods(prefix.clone()), Some(&name)) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reference_key_cases() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/claude-code-2.1.310.json")).unwrap();
        for case in fixture["keys"].as_array().unwrap() {
            let key = case["input"].as_str().unwrap();
            let flags = case["output"]["clipboard"].as_array().unwrap();
            assert_eq!(
                clipboard_flags(key),
                (
                    flags.iter().any(|v| v == "clipboardRead"),
                    flags.iter().any(|v| v == "clipboardWrite")
                ),
                "{case}"
            );
            assert_eq!(
                system_shortcut(key, true),
                case["output"]["darwinSystem"].as_bool().unwrap(),
                "{case}"
            );
            assert_eq!(
                system_shortcut(key, false),
                case["output"]["win32System"].as_bool().unwrap(),
                "{case}"
            );
        }
    }
}
