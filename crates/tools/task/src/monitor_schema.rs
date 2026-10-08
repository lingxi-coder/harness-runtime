//! Small source helpers shared by Monitor validation and permission checks.

use lingxi_core::types::utf16_json::Utf16JsonProjection;

/// Upstream's control-character check for WebSocket URL display.
pub(super) fn hidden_control(text: &str) -> bool {
    text.chars()
        .any(|c| c != '\t' && c != '\n' && ((c as u32) < 32 || (127..=159).contains(&(c as u32))))
}

#[cfg(test)]
fn has_command(value: Option<&Utf16JsonProjection>) -> bool {
    value
        .and_then(|value| value.string_units(""))
        .is_some_and(|units| !units.is_empty())
}

#[cfg(test)]
mod has_command_tests {
    use super::has_command;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    fn source(value: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::root_string(value.to_owned(), value.encode_utf16().collect())
            .expect("Rust string is a valid source string")
    }

    /// `Boolean(x) === true` for the command half of upstream's
    /// `Y(e.command, e.ws)`. The empty string is falsy, unlike whitespace.
    #[test]
    fn only_a_non_empty_string_counts_as_a_command() {
        assert!(has_command(Some(&source("echo ready"))));
        assert!(
            has_command(Some(&source(" "))),
            "whitespace is truthy in JS"
        );

        assert!(!has_command(None), "absent");
        assert!(!has_command(Some(&source(""))), "empty string is falsy");
    }
}
