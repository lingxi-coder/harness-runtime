//! Pure Cd matching and rejection text from the pinned 2.1.307 oracle.
//!
//! The caller resolves source roots and supplies relative paths. The matcher
//! follows JavaScript's UTF-16, non-Unicode, case-insensitive regular expression
//! semantics instead of applying a filesystem glob engine's different rules.

/// Result of applying Cd deny rules before allow rules.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CdCheck {
    /// The requested directory is permitted.
    Allowed,
    /// A deny rule blocked the requested directory.
    BlockedByRule {
        /// Already formatted rule, normally produced by [`format_cd_rule`].
        rule: String,
        /// Display label for the rule's source.
        source_label: String,
        /// True when `ruleContent` is absent, rather than an empty pattern.
        tool_wide: bool,
    },
    /// The directory did not match any configured allow pattern.
    OutsideAllowedPatterns {
        /// Patterns that a permitted directory must match.
        allowed_patterns: Vec<String>,
    },
}

/// Match the relative pattern and path after resolving the rule's source root.
/// Paths outside that root are rejected before matching, as in native `B`.
#[must_use]
pub fn matches_cd_pattern(relative_pattern: &str, relative_path: &str) -> bool {
    if relative_path == ".." || relative_path.starts_with("../") {
        return false;
    }
    let mut normalized = String::with_capacity(relative_pattern.len());
    let mut last_was_slash = false;
    for character in relative_pattern.chars() {
        if character != '/' || !last_was_slash {
            normalized.push(character);
        }
        last_was_slash = character == '/';
    }
    let normalized = normalized.strip_prefix('/').unwrap_or(&normalized);
    let normalized = normalized.strip_suffix('/').unwrap_or(normalized);
    native_glob_match(normalized, relative_path)
}

/// Format a native Cd rule, escaping backslashes and parentheses.
#[must_use]
pub fn format_cd_rule(pattern: Option<&str>) -> String {
    match pattern {
        None | Some("") => "Cd".to_owned(),
        Some(pattern) => format!(
            "Cd({})",
            pattern
                .replace('\\', "\\\\")
                .replace('(', "\\(")
                .replace(')', "\\)")
        ),
    }
}

/// Format native Jpr's terminal or remote rejection text.
/// An allowed check has no rejection text.
#[must_use]
pub fn format_cd_rejection(path: &str, check: &CdCheck, terminal_affordances: bool) -> String {
    match check {
        CdCheck::Allowed => String::new(),
        CdCheck::BlockedByRule {
            rule,
            source_label,
            tool_wide: true,
        } => {
            if terminal_affordances {
                format!("Can't move to {path} — /cd is turned off by the {rule} rule in {source_label}. Update the rule in /permissions to move between directories again.")
            } else {
                format!("Can't move to {path} — directory changes are turned off by the {rule} permission rule in {source_label}.")
            }
        }
        CdCheck::BlockedByRule {
            rule,
            source_label,
            tool_wide: false,
        } => {
            if terminal_affordances {
                format!("Can't move to {path} — it's excluded by the {rule} rule in {source_label}. Pick a directory outside that rule, or update it in /permissions.")
            } else {
                format!("Can't move to {path} — it's excluded by the {rule} permission rule in {source_label}. Pick a directory outside that rule.")
            }
        }
        CdCheck::OutsideAllowedPatterns { allowed_patterns } => {
            let patterns = allowed_patterns.join(", ");
            if terminal_affordances {
                format!("Can't move to {path} — /cd is limited to directories matching {patterns}. Pick a matching directory, or add a Cd rule in /permissions.")
            } else {
                format!("Can't move to {path} — directory changes are limited to {patterns}. Pick a matching directory.")
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Character {
    Literal(u16),
    Any,
    NonSlash,
}

impl Character {
    fn matches(self, unit: u16) -> bool {
        match self {
            Self::Literal(expected) => canonicalize(expected) == canonicalize(unit),
            Self::Any => !matches!(unit, 0x000a | 0x000d | 0x2028 | 0x2029),
            Self::NonSlash => unit != u16::from(b'/'),
        }
    }
}

// ECMAScript Canonicalize with Unicode=false. A multi-unit uppercase expansion
// stays unchanged; a non-ASCII character cannot fold into ASCII (e.g. long s).
fn canonicalize(unit: u16) -> u16 {
    let Some(character) = char::from_u32(u32::from(unit)) else {
        return unit;
    };
    let mut uppercase = character.to_uppercase();
    let Some(first) = uppercase.next() else {
        return unit;
    };
    if uppercase.next().is_some() {
        return unit;
    }
    let Ok(upper) = u16::try_from(u32::from(first)) else {
        return unit;
    };
    if unit >= 128 && upper < 128 {
        unit
    } else {
        upper
    }
}

#[derive(Default)]
struct State {
    epsilon: Vec<usize>,
    edges: Vec<(Character, usize)>,
}

fn closure(states: &[State], active: &mut [bool]) {
    let mut pending: Vec<_> = active
        .iter()
        .enumerate()
        .filter_map(|(index, &on)| on.then_some(index))
        .collect();
    while let Some(index) = pending.pop() {
        for &next in &states[index].epsilon {
            if !active[next] {
                active[next] = true;
                pending.push(next);
            }
        }
    }
}

fn native_glob_match(pattern: &str, value: &str) -> bool {
    let pattern: Vec<_> = pattern.encode_utf16().collect();
    let mut states = vec![State::default()];
    let mut current = 0;
    let mut at = 0;
    while at < pattern.len() {
        let unit = pattern[at];
        let next = states.len();
        states.push(State::default());
        if at == 0 && pattern.get(at..at + 3) == Some(&[42, 42, 47]) {
            // (?:.*/)?
            let repeated = states.len();
            states.push(State::default());
            states[current].epsilon.push(next);
            states[current]
                .edges
                .extend([(Character::Literal(47), next), (Character::Any, repeated)]);
            states[repeated]
                .edges
                .extend([(Character::Literal(47), next), (Character::Any, repeated)]);
            at += 3;
        } else if pattern.get(at..at + 3) == Some(&[47, 42, 42]) {
            // (/.*)?
            let repeated = states.len();
            states.push(State::default());
            states[current].epsilon.push(next);
            states[current]
                .edges
                .push((Character::Literal(47), repeated));
            states[repeated].edges.push((Character::Any, repeated));
            states[repeated].epsilon.push(next);
            at += 3;
        } else if unit == 42 && pattern.get(at + 1) == Some(&42) {
            states[current].epsilon.push(next);
            states[current].edges.push((Character::Any, current));
            at += 2;
        } else if unit == 42 {
            states[current].edges.push((Character::NonSlash, next));
            states[next].edges.push((Character::NonSlash, next));
            at += 1;
        } else {
            states[current].edges.push((Character::Literal(unit), next));
            at += 1;
        }
        current = next;
    }
    let mut active = vec![false; states.len()];
    active[0] = true;
    closure(&states, &mut active);
    for unit in value.encode_utf16() {
        let mut following = vec![false; states.len()];
        for (index, state) in states.iter().enumerate() {
            if active[index] {
                for &(character, target) in &state.edges {
                    if character.matches(unit) {
                        following[target] = true;
                    }
                }
            }
        }
        closure(&states, &mut following);
        active = following;
    }
    active[current]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../session/tests/fixtures/workspace_session_2_1_307.json"
        ))
        .unwrap()
    }

    #[test]
    fn glob_and_rooted_matching_follow_native_307_replay() {
        let fixture = oracle();
        for case in fixture["cd_glob_cases"].as_array().unwrap() {
            let input = &case["input"];
            assert_eq!(
                native_glob_match(
                    input["pattern"].as_str().unwrap(),
                    input["value"].as_str().unwrap()
                ),
                case["expected"]["matches"].as_bool().unwrap(),
                "{input}"
            );
        }
        for case in fixture["cd_matcher_cases"].as_array().unwrap() {
            assert_eq!(
                matches_cd_pattern(
                    case["relative_pattern"].as_str().unwrap(),
                    case["relative_path"].as_str().unwrap()
                ),
                case["expected"].as_bool().unwrap(),
                "{}",
                case["input"]
            );
        }
    }

    #[test]
    fn rejection_text_follows_native_307_replay() {
        for case in oracle()["cd_rejection_cases"].as_array().unwrap() {
            let input = &case["input"];
            let native = &input["check"];
            let check = if native["result"] == "blockedByRule" {
                let rule = &native["rule"];
                let content = rule["ruleValue"].get("ruleContent").and_then(Value::as_str);
                let source_label = match rule["source"].as_str().unwrap() {
                    "userSettings" => "user settings",
                    "projectSettings" => "shared project settings",
                    "localSettings" => "project local settings",
                    "flagSettings" => "command line arguments",
                    "policySettings" => "enterprise managed settings",
                    "session" => "current session",
                    other => panic!("unhandled fixture source {other}"),
                };
                CdCheck::BlockedByRule {
                    rule: format_cd_rule(content),
                    source_label: source_label.to_owned(),
                    tool_wide: content.is_none(),
                }
            } else {
                CdCheck::OutsideAllowedPatterns {
                    allowed_patterns: native["allowedPatterns"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|pattern| pattern.as_str().unwrap().to_owned())
                        .collect(),
                }
            };
            assert_eq!(
                format_cd_rejection(
                    input["path"].as_str().unwrap(),
                    &check,
                    input["terminal_affordances"].as_bool().unwrap()
                ),
                case["expected"].as_str().unwrap(),
                "{input}"
            );
        }
    }
}
