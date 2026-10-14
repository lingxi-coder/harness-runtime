//! Native 2.1.295's `util.stripVTControlCharacters` behavior for policy errors.
//!
//! Keep this source-specific matcher separate from the shared terminal
//! sanitizer: the Node-compatible regex intentionally has different handling
//! for incomplete sequences and C1 controls.

use std::sync::LazyLock;

use fancy_regex::Regex;

// Native imports `stripVTControlCharacters` from `util`. Bun 1.4.3 exposes the
// Node 26.3.0 matcher below. Spell JavaScript's non-Unicode `\d` classes as
// `[0-9]`: Rust's `\d` also matches non-ASCII decimal digits.
const NATIVE_VT_CONTROL_PATTERN: &str = r"[\x1B\x9B][\x5B\x5D\x28\x29#;?]*(?:(?:(?:(?:;[-a-zA-Z0-9/#&.:=?%@~_]+)*|[a-zA-Z0-9]+(?:;[-a-zA-Z0-9/#&.:=?%@~_]*)*)?(?:\x07|\x1B\\|\x9C))|(?:(?:[0-9]{1,4}(?:;[0-9]{0,4})*)?[0-9A-PR-TZcf-nq-uy=><~]))";

static NATIVE_VT_CONTROL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(NATIVE_VT_CONTROL_PATTERN).expect("the fixed Native VT-control pattern must compile")
});

/// Strip the exact VT sequences recognized by Native 2.1.295's
/// `util.stripVTControlCharacters` implementation.
///
/// This is only the `t(Lw(value))` stage of Native's `zd` helper. Lone
/// surrogate cleanup and the later Cc/Cf/U+2028/U+2029 filter remain separate
/// operations in the policy-helper path.
pub(super) fn strip_native_vt_control_characters(value: &str) -> String {
    NATIVE_VT_CONTROL.replace_all(value, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::strip_native_vt_control_characters;

    // These are the stripVTControlCharacters intermediate outputs used by the
    // byte-bound Native `zd` wrapper. The wrapper's Lw and trailing category
    // cleanup are separate stages; all fixture inputs here are ASCII except
    // the explicit C1 controls, so Lw is the identity.
    #[test]
    fn matches_native_295_vt_stripper_source_fixtures() {
        let fixtures = [
            ("x\u{1b}[31mred\u{1b}[0my", "xredy"),
            ("x\u{1b}[31", "x"),
            ("x\u{1b}[", "x\u{1b}["),
            ("x\u{1b}]0;title\u{7}y", "xy"),
            ("x\u{1b}]0;title\u{1b}\\y", "xy"),
            ("x\u{1b}]0;title", "xitle"),
            ("x\u{1b}P1;2|data\u{1b}\\y", "x1;2|data\u{1b}\\y"),
            ("x\u{1b}P1;2|data\u{7}y", "x1;2|data\u{7}y"),
            ("x\u{1b}P1;2|data", "x1;2|data"),
            ("x\u{1b}Xdata\u{1b}\\y", "xy"),
            ("x\u{1b}^data\u{1b}\\y", "x\u{1b}^data\u{1b}\\y"),
            ("x\u{1b}_data\u{1b}\\y", "x\u{1b}_data\u{1b}\\y"),
            ("x\u{1b}7y", "x"),
            ("x\u{1b}(0y", "x"),
            ("x\u{1b}?y", "x"),
            ("x\u{9b}31mred\u{9b}0my", "xredy"),
            ("x\u{9d}0;title\u{7}y", "x\u{9d}0;title\u{7}y"),
            ("x\u{9d}0;title\u{9c}y", "x\u{9d}0;title\u{9c}y"),
            ("x\u{90}data\u{9c}y", "x\u{90}data\u{9c}y"),
            ("x\u{98}data\u{9c}y", "x\u{98}data\u{9c}y"),
            ("x\u{9c}y", "x\u{9c}y"),
        ];

        for (input, expected) in fixtures {
            assert_eq!(
                strip_native_vt_control_characters(input),
                expected,
                "input code points: {:?}",
                input.chars().map(u32::from).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn javascript_non_unicode_digit_classes_are_ascii_only() {
        let input = "x\u{1b}[١31m";

        assert_eq!(strip_native_vt_control_characters(input), input);
    }
}
