//! Input validation, byte-verified against 2.1.218 binary strings where a
//! concrete fragment was recovered (the message text is exact; the `code`
//! the binary attaches alongside each message — `bad_args`, `state_conflict`,
//! etc. — is noted in a comment rather than reproduced structurally, since
//! `tool_api::ToolError` has no separate code channel to carry it in).

use serde_json::Value;
use tool_api::tool_trait::ToolError;

/// `coordinate is required` / `coordinate must be an array of length 2` /
/// `coordinate must be a tuple of non-negative numbers` (`bad_args`).
pub fn require_coord(input: &Value, key: &str) -> Result<(u32, u32), ToolError> {
    let Some(v) = input.get(key) else {
        // `coordinate` has a legacy flat x/y fallback the binary doesn't —
        // check that before erroring "required".
        if key == "coordinate" {
            let x = input.get("x").and_then(Value::as_u64);
            let y = input.get("y").and_then(Value::as_u64);
            if let (Some(x), Some(y)) = (x, y) {
                #[allow(clippy::cast_possible_truncation)]
                return Ok((x as u32, y as u32));
            }
        }
        return Err(ToolError::InvalidInput(format!("{key} is required")));
    };
    let Some(arr) = v.as_array() else {
        return Err(ToolError::InvalidInput(format!(
            "{key} must be an array of length 2"
        )));
    };
    if arr.len() != 2 {
        return Err(ToolError::InvalidInput(format!(
            "{key} must be an array of length 2"
        )));
    }
    let (Some(x), Some(y)) = (arr[0].as_u64(), arr[1].as_u64()) else {
        return Err(ToolError::InvalidInput(format!(
            "{key} must be a tuple of non-negative numbers"
        )));
    };
    #[allow(clippy::cast_possible_truncation)] // coordinate space never exceeds u32
    Ok((x as u32, y as u32))
}

/// `text is required` (`bad_args`) / `text must be a string` (`bad_args`).
pub fn require_text(input: &Value) -> Result<String, ToolError> {
    match input.get("text") {
        None => Err(ToolError::InvalidInput("text is required".into())),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(ToolError::InvalidInput("text must be a string".into())),
    }
}

/// `repeat must be a positive integer` / `repeat exceeds maximum of 100`
/// (`bad_args`). Default `1` when omitted.
pub fn key_repeat(input: &Value) -> Result<u32, ToolError> {
    let Some(v) = input.get("repeat") else {
        return Ok(1);
    };
    let Some(n) = v.as_i64() else {
        return Err(ToolError::InvalidInput(
            "repeat must be a positive integer".into(),
        ));
    };
    if n < 1 {
        return Err(ToolError::InvalidInput(
            "repeat must be a positive integer".into(),
        ));
    }
    if n > 100 {
        return Err(ToolError::InvalidInput(
            "repeat exceeds maximum of 100".into(),
        ));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // bounded to [1, 100] above
    Ok(n as u32)
}

/// `region must be an array of length 4: [x0, y0, x1, y1]` / `region values
/// must be non-negative numbers` / `region x1 must be greater than x0` /
/// `region y1 must be greater than y0` (all `bad_args`). Returns
/// `(x0, y0, x1, y1)`.
pub fn require_region(input: &Value) -> Result<(u32, u32, u32, u32), ToolError> {
    let Some(arr) = input.get("region").and_then(Value::as_array) else {
        return Err(ToolError::InvalidInput(
            "region must be an array of length 4: [x0, y0, x1, y1]".into(),
        ));
    };
    if arr.len() != 4 {
        return Err(ToolError::InvalidInput(
            "region must be an array of length 4: [x0, y0, x1, y1]".into(),
        ));
    }
    let vals: Option<Vec<u64>> = arr.iter().map(Value::as_u64).collect();
    let Some(vals) = vals else {
        return Err(ToolError::InvalidInput(
            "region values must be non-negative numbers".into(),
        ));
    };
    // Reject anything outside u32 range BEFORE the x1>x0/y1>y0 comparison and
    // the truncating cast below. Comparing in u64 space and only then
    // truncating to u32 is unsound: e.g. x0=2, x1=4_294_967_297 passes
    // `x1 > x0` in u64, but `4_294_967_297 as u32 == 1` — the truncated x1
    // ends up LESS than x0, and the zoom call site's `x1 - x0` on the
    // (now-corrupted) u32 pair underflows/panics.
    if vals.iter().any(|v| *v > u64::from(u32::MAX)) {
        return Err(ToolError::InvalidInput(
            "region values must be non-negative numbers".into(),
        ));
    }
    let (x0, y0, x1, y1) = (vals[0], vals[1], vals[2], vals[3]);
    if x1 <= x0 {
        return Err(ToolError::InvalidInput(
            "region x1 must be greater than x0".into(),
        ));
    }
    if y1 <= y0 {
        return Err(ToolError::InvalidInput(
            "region y1 must be greater than y0".into(),
        ));
    }
    #[allow(clippy::cast_possible_truncation)] // every value checked <= u32::MAX above
    Ok((x0 as u32, y0 as u32, x1 as u32, y1 as u32))
}

/// Shared duration validation for `wait`/`hold_key`: `duration must be a
/// number` / `duration must be non-negative` / `duration is too long.
/// Duration is in seconds.` (`bad_args`). `max_secs` is the action-specific
/// ceiling — the binary's own cutoff wasn't recoverable byte-for-byte from
/// strings alone, so 60s (matching the prior `LingXi` clamp) is kept as a
/// documented, reasonable bound rather than a verified one.
fn duration_secs(input: &Value, max_secs: f64) -> Result<f64, ToolError> {
    let Some(v) = input.get("duration") else {
        return Err(ToolError::InvalidInput("duration must be a number".into()));
    };
    let Some(n) = v.as_f64() else {
        return Err(ToolError::InvalidInput("duration must be a number".into()));
    };
    if n < 0.0 {
        return Err(ToolError::InvalidInput(
            "duration must be non-negative".into(),
        ));
    }
    if n > max_secs {
        return Err(ToolError::InvalidInput(
            "duration is too long. Duration is in seconds.".into(),
        ));
    }
    Ok(n)
}

/// `wait`'s duration validation.
pub fn wait_duration(input: &Value) -> Result<f64, ToolError> {
    duration_secs(input, 60.0)
}

/// `hold_key`'s duration validation (same rules, kept as a distinct entry
/// point in case the two ceilings ever diverge).
pub fn hold_duration(input: &Value) -> Result<f64, ToolError> {
    duration_secs(input, 60.0)
}

/// `scroll_direction must be 'up', 'down', 'left', or 'right'` (`bad_args`).
/// Reads `scroll_direction` first, falling back to the legacy `direction`
/// key. Returns `None` when neither is present (caller falls back to flat
/// `dx`/`dy`), `Err` when one IS present but isn't one of the four values.
pub fn scroll_direction(input: &Value) -> Result<Option<&str>, ToolError> {
    let Some(dir) = input
        .get("scroll_direction")
        .or_else(|| input.get("direction"))
    else {
        return Ok(None);
    };
    match dir.as_str() {
        Some(d @ ("up" | "down" | "left" | "right")) => Ok(Some(d)),
        _ => Err(ToolError::InvalidInput(
            "scroll_direction must be 'up', 'down', 'left', or 'right'".into(),
        )),
    }
}

/// `scroll_amount must be a non-negative int` / `scroll_amount exceeds
/// maximum of 100` (`bad_args`). Reads `scroll_amount` first, falling back to
/// the legacy `amount` key. Defaults to `3` (one tick) when neither is
/// present.
pub fn scroll_amount(input: &Value) -> Result<i32, ToolError> {
    let Some(v) = input.get("scroll_amount").or_else(|| input.get("amount")) else {
        return Ok(3);
    };
    let Some(n) = v.as_i64() else {
        return Err(ToolError::InvalidInput(
            "scroll_amount must be a non-negative int".into(),
        ));
    };
    if n < 0 {
        return Err(ToolError::InvalidInput(
            "scroll_amount must be a non-negative int".into(),
        ));
    }
    if n > 100 {
        return Err(ToolError::InvalidInput(
            "scroll_amount exceeds maximum of 100".into(),
        ));
    }
    #[allow(clippy::cast_possible_truncation)] // bounded to [0, 100] above
    Ok(n as i32)
}

/// Canonicalize one modifier segment to a stable spelling, so `cmd`,
/// `command`, `meta`, `super`, `win`, and `windows` (all synonyms `enigo`
/// itself accepts — see `platforms/macos-computer-control`'s `keymap.rs`)
/// can't be used to dodge a modifier-based match. `None` for anything that
/// isn't a recognized modifier name.
fn canonical_modifier(part: &str) -> Option<&'static str> {
    match part.to_ascii_lowercase().as_str() {
        "cmd" | "command" | "meta" | "super" | "win" | "windows" => Some("cmd"),
        "ctrl" | "control" => Some("ctrl"),
        "alt" | "option" | "opt" => Some("alt"),
        "shift" => Some("shift"),
        _ => None,
    }
}

/// Parse a `+`-joined chord into a sorted, deduped, canonical modifier set
/// plus a canonical main-key name. Sorting makes the match order-insensitive
/// (`cmd+ctrl+q` and `ctrl+cmd+q` compare equal); canonicalizing the last
/// segment folds the couple of key aliases most likely to matter for a
/// system shortcut (`esc`/`escape`, `enter`/`return`). Returns `None` for an
/// empty chord (nothing to protect).
fn canonicalize_chord(chord: &str) -> Option<(Vec<&'static str>, String)> {
    let mut parts: Vec<&str> = chord
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let last = parts.pop()?;
    let main = match last.to_ascii_lowercase().as_str() {
        "esc" => "escape".to_string(),
        "enter" => "return".to_string(),
        other => other.to_string(),
    };
    let mut mods: Vec<&'static str> = parts.iter().filter_map(|p| canonical_modifier(p)).collect();
    mods.sort_unstable();
    mods.dedup();
    Some((mods, main))
}

/// Common macOS system-level shortcuts that need the `systemKeyCombos` grant
/// regardless of the frontmost app's tier. Modifier-order- and
/// -synonym-insensitive (matched via [`canonicalize_chord`]) so a respelling
/// like `"command+q"` or a reordering like `"cmd+ctrl+q"` (for the canonical
/// `"ctrl+cmd+q"`) can't slip past the grant check. Best-effort, documented
/// as an approximation (see module docs) — not a byte-exact port of the
/// binary's own detector. Modifier lists below are pre-sorted alphabetically
/// (`alt` < `cmd` < `ctrl` < `shift`) to match `canonicalize_chord`'s output.
pub fn is_system_shortcut(chord: &str) -> bool {
    const SYSTEM_CHORDS: &[(&[&str], &str)] = &[
        (&["cmd"], "q"),
        (&["cmd"], "tab"),
        (&["cmd", "shift"], "tab"),
        (&["cmd"], "space"),
        (&["cmd"], "h"),
        (&["cmd"], "m"),
        (&["alt", "cmd"], "escape"),
        (&["cmd", "ctrl"], "q"),
        (&["cmd", "shift"], "q"),
        (&["ctrl"], "up"),
        (&["ctrl"], "down"),
        (&["alt", "cmd"], "d"),
    ];
    let Some((mods, main)) = canonicalize_chord(chord) else {
        return false;
    };
    SYSTEM_CHORDS
        .iter()
        .any(|(m, k)| mods.as_slice() == *m && *k == main)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn coord_missing_is_required() {
        let e = require_coord(&json!({}), "coordinate").unwrap_err();
        assert_eq!(e.to_string(), "invalid input: coordinate is required");
    }

    #[test]
    fn coord_wrong_length() {
        let e = require_coord(&json!({ "coordinate": [1] }), "coordinate").unwrap_err();
        assert_eq!(
            e.to_string(),
            "invalid input: coordinate must be an array of length 2"
        );
    }

    #[test]
    fn coord_non_numeric() {
        let e = require_coord(&json!({ "coordinate": ["a", "b"] }), "coordinate").unwrap_err();
        assert_eq!(
            e.to_string(),
            "invalid input: coordinate must be a tuple of non-negative numbers"
        );
    }

    #[test]
    fn text_required_and_type_checked() {
        assert_eq!(
            require_text(&json!({})).unwrap_err().to_string(),
            "invalid input: text is required"
        );
        assert_eq!(
            require_text(&json!({ "text": 5 })).unwrap_err().to_string(),
            "invalid input: text must be a string"
        );
        assert_eq!(require_text(&json!({ "text": "hi" })).unwrap(), "hi");
    }

    #[test]
    fn repeat_defaults_and_validates() {
        assert_eq!(key_repeat(&json!({})).unwrap(), 1);
        assert_eq!(key_repeat(&json!({ "repeat": 5 })).unwrap(), 5);
        assert_eq!(
            key_repeat(&json!({ "repeat": 0 })).unwrap_err().to_string(),
            "invalid input: repeat must be a positive integer"
        );
        assert_eq!(
            key_repeat(&json!({ "repeat": 101 }))
                .unwrap_err()
                .to_string(),
            "invalid input: repeat exceeds maximum of 100"
        );
    }

    #[test]
    fn region_full_validation_chain() {
        assert_eq!(
            require_region(&json!({})).unwrap_err().to_string(),
            "invalid input: region must be an array of length 4: [x0, y0, x1, y1]"
        );
        assert_eq!(
            require_region(&json!({ "region": [1, 2, 3] }))
                .unwrap_err()
                .to_string(),
            "invalid input: region must be an array of length 4: [x0, y0, x1, y1]"
        );
        assert_eq!(
            require_region(&json!({ "region": [0, 0, 0, 10] }))
                .unwrap_err()
                .to_string(),
            "invalid input: region x1 must be greater than x0"
        );
        assert_eq!(
            require_region(&json!({ "region": [0, 0, 10, 0] }))
                .unwrap_err()
                .to_string(),
            "invalid input: region y1 must be greater than y0"
        );
        assert_eq!(
            require_region(&json!({ "region": [1, 2, 11, 22] })).unwrap(),
            (1, 2, 11, 22)
        );
    }

    #[test]
    fn region_rejects_values_that_would_wrap_past_u32_and_corrupt_the_x1_gt_x0_check() {
        // 4_294_967_297 == 2^32 + 1, which truncates to 1 as u32 — smaller
        // than x0=2. Comparing in u64 space (as the function does) must catch
        // this as out-of-range BEFORE truncating, not let it slip through as
        // a "valid" (x0=2, x1=1) pair that then underflows at the call site.
        let e = require_region(&json!({ "region": [2, 0, 4_294_967_297u64, 10] })).unwrap_err();
        assert_eq!(
            e.to_string(),
            "invalid input: region values must be non-negative numbers"
        );
    }

    #[test]
    fn duration_full_validation_chain() {
        assert_eq!(
            wait_duration(&json!({})).unwrap_err().to_string(),
            "invalid input: duration must be a number"
        );
        assert_eq!(
            wait_duration(&json!({ "duration": -1 }))
                .unwrap_err()
                .to_string(),
            "invalid input: duration must be non-negative"
        );
        assert_eq!(
            wait_duration(&json!({ "duration": 9999 }))
                .unwrap_err()
                .to_string(),
            "invalid input: duration is too long. Duration is in seconds."
        );
        assert!((wait_duration(&json!({ "duration": 2.5 })).unwrap() - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn scroll_direction_validates_and_falls_back() {
        assert_eq!(scroll_direction(&json!({})).unwrap(), None);
        assert_eq!(
            scroll_direction(&json!({ "scroll_direction": "up" })).unwrap(),
            Some("up")
        );
        assert_eq!(
            scroll_direction(&json!({ "direction": "left" })).unwrap(),
            Some("left")
        );
        assert_eq!(
            scroll_direction(&json!({ "scroll_direction": "sideways" }))
                .unwrap_err()
                .to_string(),
            "invalid input: scroll_direction must be 'up', 'down', 'left', or 'right'"
        );
    }

    #[test]
    fn scroll_amount_defaults_and_bounds() {
        assert_eq!(scroll_amount(&json!({})).unwrap(), 3);
        assert_eq!(scroll_amount(&json!({ "scroll_amount": 50 })).unwrap(), 50);
        assert_eq!(scroll_amount(&json!({ "amount": 10 })).unwrap(), 10);
        assert_eq!(
            scroll_amount(&json!({ "scroll_amount": -1 }))
                .unwrap_err()
                .to_string(),
            "invalid input: scroll_amount must be a non-negative int"
        );
        assert_eq!(
            scroll_amount(&json!({ "scroll_amount": 101 }))
                .unwrap_err()
                .to_string(),
            "invalid input: scroll_amount exceeds maximum of 100"
        );
    }

    #[test]
    fn system_shortcut_detection() {
        assert!(is_system_shortcut("cmd+q"));
        assert!(is_system_shortcut("CMD+TAB"));
        assert!(!is_system_shortcut("cmd+a"));
        assert!(!is_system_shortcut("a"));
    }

    #[test]
    fn system_shortcut_detection_is_not_bypassable_by_synonym() {
        // "command" is a synonym enigo itself accepts for "cmd" — must still
        // be caught, not just the literal spelling "cmd".
        assert!(is_system_shortcut("command+q"));
        assert!(is_system_shortcut("meta+q"));
        assert!(is_system_shortcut("super+q"));
        assert!(is_system_shortcut("win+q"));
    }

    #[test]
    fn system_shortcut_detection_is_not_bypassable_by_reordering() {
        // Canonical entry is "ctrl+cmd+q" — the reordered spelling must match
        // the same chord, not slip through as an unrecognized combination.
        assert!(is_system_shortcut("cmd+ctrl+q"));
        assert!(is_system_shortcut("CTRL+CMD+Q"));
    }

    #[test]
    fn system_shortcut_detection_folds_key_aliases() {
        assert!(is_system_shortcut("cmd+opt+esc"));
        assert!(is_system_shortcut("cmd+alt+escape"));
    }
}
