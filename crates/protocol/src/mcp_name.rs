//! Shared MCP server-name normalization used by registry and permission code.

const CLAUDEAI_SERVER_PREFIX: &str = "claude.ai ";

/// Normalize a server name to the token used by `mcp__<server>__<tool>`.
#[must_use]
pub fn normalize_name_for_mcp(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    for character in name.chars() {
        if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
            normalized.push(character);
        } else {
            // claude-code `Ll` uses `String.replace(/[^a-zA-Z0-9_-]/g,"_")`,
            // which runs per UTF-16 CODE UNIT — a non-BMP character (an emoji
            // and other astral-plane scalars are a surrogate PAIR) becomes TWO
            // underscores, not one. Mirror that with `len_utf16()`.
            for _ in 0..character.len_utf16() {
                normalized.push('_');
            }
        }
    }
    if name.starts_with(CLAUDEAI_SERVER_PREFIX) {
        normalized = collapse_and_trim_underscores(&normalized);
    }
    normalized
}

fn collapse_and_trim_underscores(value: &str) -> String {
    let mut collapsed = String::with_capacity(value.len());
    let mut previous_underscore = false;
    for character in value.chars() {
        if character == '_' {
            if !previous_underscore {
                collapsed.push('_');
            }
            previous_underscore = true;
        } else {
            collapsed.push(character);
            previous_underscore = false;
        }
    }
    let trimmed = collapsed
        .strip_prefix('_')
        .unwrap_or(&collapsed)
        .to_string();
    trimmed.strip_suffix('_').unwrap_or(&trimmed).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_ai_names_collapse_delimiter_runs() {
        assert_eq!(normalize_name_for_mcp("claude.ai .a..b "), "claude_ai_a_b");
    }

    #[test]
    fn ordinary_names_preserve_repeated_underscores() {
        assert_eq!(normalize_name_for_mcp("a..b"), "a__b");
    }

    #[test]
    fn astral_plane_character_becomes_two_underscores() {
        // 😀 (U+1F600) is a UTF-16 surrogate pair, so the oracle's per-code-unit
        // replace emits TWO underscores for it (BMP chars stay one).
        assert_eq!(normalize_name_for_mcp("a😀b"), "a__b");
        assert_eq!(normalize_name_for_mcp("a€b"), "a_b"); // U+20AC is BMP → one
    }
}
