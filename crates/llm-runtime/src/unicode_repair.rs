//! Exact UTF-16 input repair from Claude Code 2.1.292 `ZDe` / `QDe`.
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonBuilder, Utf16JsonProjection};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RepairStats {
    pub repaired_strings: u32,
    pub windows_path_skips: u32,
    pub control_escape_skips: u32,
    pub proto_keys: u32,
}
impl RepairStats {
    #[must_use]
    pub fn is_noteworthy(self) -> bool {
        self.repaired_strings > 0
            || self.windows_path_skips > 0
            || self.control_escape_skips > 0
            || self.proto_keys > 0
    }
}
fn letter(u: u16) -> bool {
    matches!(u, 0x41..=0x5a | 0x61..=0x7a)
}
fn js_whitespace(u: u16) -> bool {
    matches!(u, 0x9..=0xd | 0x20 | 0xa0 | 0x1680 | 0x2000..=0x200a
        | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff)
}
fn windows_path(units: &[u16]) -> bool {
    for index in 0..units.len() {
        if index + 2 < units.len()
            && letter(units[index])
            && units[index + 1] == 0x3a
            && matches!(units[index + 2], 0x5c | 0x2f)
            && (index == 0 || !letter(units[index - 1]))
        {
            return true;
        }
        if units.get(index..index + 2) != Some(&[0x5c, 0x5c]) {
            continue;
        }
        if index > 0
            && !js_whitespace(units[index - 1])
            && !matches!(units[index - 1], 0x22 | 0x27 | 0x3d)
        {
            continue;
        }
        let start = index + 2;
        let mut end = start;
        while end < units.len() && !js_whitespace(units[end]) && !matches!(units[end], 0x5c | 0x2f)
        {
            end += 1;
        }
        if end > start
            && end < units.len()
            && matches!(units[end], 0x5c | 0x2f)
            && units.get(end + 1) != Some(&0x5c)
        {
            return true;
        }
    }
    false
}
#[must_use]
pub fn looks_like_windows_path(value: &str) -> bool {
    windows_path(&value.encode_utf16().collect::<Vec<_>>())
}
fn escaped_unit(units: &[u16], index: usize) -> Option<u16> {
    let spelling = units.get(index..index + 6)?;
    if spelling[..2] != [0x5c, 0x75] {
        return None;
    }
    let mut value = 0;
    for unit in &spelling[2..] {
        let digit = match unit {
            0x30..=0x39 => unit - 0x30,
            0x41..=0x46 => unit - 0x41 + 10,
            0x61..=0x66 => unit - 0x61 + 10,
            _ => return None,
        };
        value = (value << 4) | digit;
    }
    Some(value)
}
fn repair_units(units: &[u16], java_script_source: bool, stats: &mut RepairStats) -> Vec<u16> {
    if !(0..units.len()).any(|index| escaped_unit(units, index).is_some()) {
        return units.to_vec();
    }
    if windows_path(units) {
        stats.windows_path_skips += 1;
        return units.to_vec();
    }
    let mut output = Vec::with_capacity(units.len());
    let mut index = 0;
    let mut control_skipped = false;
    while index < units.len() {
        let Some(unit) = escaped_unit(units, index) else {
            output.push(units[index]);
            index += 1;
            continue;
        };
        let low = if (0xd800..=0xdbff).contains(&unit) {
            escaped_unit(units, index + 6).filter(|next| (0xdc00..=0xdfff).contains(next))
        } else {
            None
        };
        let length = if low.is_some() { 12 } else { 6 };
        let mut preceding = index;
        while preceding > 0 && units[preceding - 1] == 0x5c {
            preceding -= 1;
        }
        if (index - preceding) & 1 == 1 {
            output.extend_from_slice(&units[index..index + length]);
        } else if let Some(low) = low {
            output.extend_from_slice(&[unit, low]);
        } else if (0xd800..=0xdfff).contains(&unit) {
            output.extend_from_slice(&units[index..index + length]);
        } else if unit <= 31 || (127..=159).contains(&unit) {
            control_skipped = true;
            output.extend_from_slice(&units[index..index + length]);
        } else if java_script_source && (unit < 128 || unit == 8232 || unit == 8233) {
            output.extend_from_slice(&units[index..index + length]);
        } else {
            output.push(unit);
        }
        index += length;
    }
    if output != units {
        stats.repaired_strings += 1;
    }
    if control_skipped {
        stats.control_escape_skips += 1;
    }
    output
}
/// Iteratively repair values. Removed `__proto__` members are not traversed.
/// Other exact keys and binary64 numbers survive in the flat arena.
#[must_use]
pub fn repair_projection(
    input: &Utf16JsonProjection,
    java_script_source: bool,
    stats: &mut RepairStats,
) -> Utf16JsonProjection {
    enum Task<'a> {
        Visit(JsonValueView<'a>),
        Array(usize),
        Object(Vec<Vec<u16>>),
    }
    let mut builder = Utf16JsonBuilder::new();
    let mut complete = Vec::new();
    let mut pending = vec![Task::Visit(
        input.at_pointer("").expect("valid projection root"),
    )];
    while let Some(task) = pending.pop() {
        match task {
            Task::Array(length) => {
                let items = complete.split_off(complete.len() - length);
                complete.push(builder.push_array(items).expect("completed child indices"));
            }
            Task::Object(keys) => {
                let items = complete.split_off(complete.len() - keys.len());
                complete.push(
                    builder
                        .push_object_units(keys.into_iter().zip(items).collect())
                        .expect("completed object child indices"),
                );
            }
            Task::Visit(value) => {
                if let Some(units) = value.code_units() {
                    complete.push(builder.push_string_units(repair_units(
                        units,
                        java_script_source,
                        stats,
                    )));
                } else if let Some(length) = value.array_len() {
                    pending.push(Task::Array(length));
                    pending.extend((0..length).rev().map(|index| {
                        Task::Visit(value.get_index(index).expect("existing array index"))
                    }));
                } else if let Some(entries) = value.object_entries() {
                    let entries = entries
                        .into_iter()
                        .filter(|entry| {
                            if entry.code_units
                                == [0x5f, 0x5f, 0x70, 0x72, 0x6f, 0x74, 0x6f, 0x5f, 0x5f]
                            {
                                stats.proto_keys += 1;
                                false
                            } else {
                                true
                            }
                        })
                        .collect::<Vec<_>>();
                    pending.push(Task::Object(
                        entries
                            .iter()
                            .map(|entry| entry.code_units.to_vec())
                            .collect(),
                    ));
                    pending.extend(
                        entries
                            .into_iter()
                            .rev()
                            .map(|entry| Task::Visit(entry.value)),
                    );
                } else if let Some(number) = value.as_number() {
                    complete.push(builder.push_number(number));
                } else if let Some(boolean) = value.as_bool() {
                    complete.push(builder.push_bool(boolean));
                } else {
                    debug_assert!(value.is_null());
                    complete.push(builder.push_null());
                }
            }
        }
    }
    builder
        .finish(complete.pop().expect("completed root"))
        .expect("valid repaired flat arena")
}
/// Current object/array tool boundary. Native `Wre` applies schema coercion
/// separately before Unicode repair, then restores a string Workflow.script.
#[must_use]
pub fn repair_tool_input(
    tool_name: &str,
    input: &Utf16JsonProjection,
) -> (Utf16JsonProjection, RepairStats) {
    if input.object_entries().is_none()
        && input
            .at_pointer("")
            .and_then(|value| value.array_len())
            .is_none()
    {
        return (input.clone(), RepairStats::default());
    }
    let mut stats = RepairStats::default();
    let mut repaired = repair_projection(input, false, &mut stats);
    if tool_name == "Workflow"
        && input
            .get_property("script")
            .is_some_and(|v| v.code_units().is_some())
    {
        repaired
            .set_field(
                "script",
                input.subprojection("/script").expect("existing script"),
            )
            .expect("object repair preserves script member");
    }
    if stats.is_noteworthy() {
        telemetry::emit_repair_double_escaped_unicode(
            stats.repaired_strings,
            stats.windows_path_skips,
            stats.control_escape_skips,
            stats.proto_keys,
        );
    }
    (repaired, stats)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn parse(source: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(source).unwrap()
    }
    #[test]
    fn lone_units_pairs_and_escape_parity_are_exact() {
        let input = parse(
            r#"{"a":"\ud800\\u0041\udc00","b":"\\ud83d\\ude00","c":"\\ud800","d":"\\\\u0041","e":"\\\\\\u0041"}"#,
        );
        let (out, stats) = repair_tool_input("Bash", &input);
        assert_eq!(out.string_units("/a"), Some(vec![0xd800, 0x41, 0xdc00]));
        assert_eq!(out.string_units("/b"), Some(vec![0xd83d, 0xde00]));
        assert_eq!(out.string_at("/c"), Some("\\ud800"));
        assert_eq!(out.string_at("/d"), Some("\\\\u0041"));
        assert_eq!(out.string_at("/e"), Some("\\\\A"));
        assert_eq!(stats.repaired_strings, 3);
    }
    #[test]
    fn controls_keep_spelling_and_count_once_per_string() {
        let input = parse(r#"{"a":"\\u0000\\u001f\\u007f\\u009f\\u0041","b":"\\u000a"}"#);
        let (out, stats) = repair_tool_input("Bash", &input);
        assert_eq!(out.string_at("/a"), Some("\\u0000\\u001f\\u007f\\u009fA"));
        assert_eq!(stats.control_escape_skips, 2);
        assert_eq!(stats.repaired_strings, 1);
    }
    #[test]
    fn javascript_source_preserves_ascii_and_line_separator_escapes() {
        let input = parse(r#""\\u0041\\u2028\\u2029\\u4f60""#);
        let mut stats = RepairStats::default();
        let out = repair_projection(&input, true, &mut stats);
        assert_eq!(out.string_at(""), Some("\\u0041\\u2028\\u2029你"));
        assert_eq!(stats.repaired_strings, 1);
    }
    #[test]
    fn windows_path_skip_uses_ecmascript_whitespace() {
        let input = parse(
            r#"{"a":"C:\\users\\u0041","b":"\ufeff\\\\host\\share\\u0041","c":"\\\\ho\u00a0st\\share\\u0041"}"#,
        );
        let (out, stats) = repair_tool_input("Bash", &input);
        assert_eq!(out.string_units("/a"), input.string_units("/a"));
        assert_eq!(out.string_units("/b"), input.string_units("/b"));
        assert_eq!(out.string_at("/c"), Some("\\\\ho\u{a0}st\\shareA"));
        assert_eq!(stats.windows_path_skips, 2);
        assert!(!looks_like_windows_path("abcC:\\x"));
        assert!(!looks_like_windows_path("\u{85}\\\\host\\share\\u0041"));
    }
    #[test]
    fn proto_key_removal_skips_removed_subtrees() {
        let input = parse(
            r#"{"__proto__":{"value":"\\u0041"},"nested":{"__proto__":"\\u0042","good":"\\u0043"},"\ud800":"\udc00"}"#,
        );
        let (out, stats) = repair_tool_input("Bash", &input);
        assert_eq!(stats.proto_keys, 2);
        assert_eq!(stats.repaired_strings, 1);
        assert!(!out.contains_property("__proto__"));
        assert_eq!(out.string_at("/nested/good"), Some("C"));
        assert!(out
            .get_property("nested")
            .unwrap()
            .get_property("__proto__")
            .is_none());
        assert_eq!(
            out.subprojection_property_units(&[0xd800])
                .unwrap()
                .string_units(""),
            Some(vec![0xdc00])
        );
    }
    #[test]
    fn workflow_restores_exact_string_and_repairs_nonstring_script() {
        let input = parse(r#"{"script":"\ud800\\u0041","other":"\\u0042"}"#);
        let (out, stats) = repair_tool_input("Workflow", &input);
        assert_eq!(out.string_units("/script"), input.string_units("/script"));
        assert_eq!(out.string_at("/other"), Some("B"));
        assert_eq!(stats.repaired_strings, 2);
        assert_eq!(
            repair_tool_input("Workflow", &parse(r#"{"script":["\\u0041"]}"#))
                .0
                .string_at("/script/0"),
            Some("A")
        );
    }
    #[test]
    fn nonobject_input_is_verbatim() {
        for source in [r#""\\u0041""#, "null", "-0", "1e400"] {
            let input = parse(source);
            let (out, stats) = repair_tool_input("Bash", &input);
            assert_eq!(out, input);
            assert_eq!(stats, RepairStats::default());
        }
    }
    #[test]
    fn deep_inputs_keep_binary64_and_do_not_own_recursive_trees() {
        let depth = 1536;
        let source = format!(
            r#"{{"nested":{}"\\u0041"{},"overflow":1e400,"zero":-0}}"#,
            "[".repeat(depth),
            "]".repeat(depth)
        );
        let input = parse(&source);
        let (out, stats) = repair_tool_input("Bash", &input);
        assert_eq!(
            out.string_at(&format!("/nested{}", "/0".repeat(depth))),
            Some("A")
        );
        assert_eq!(out.number_at("/overflow"), Some(f64::INFINITY));
        assert!(out.number_at("/zero").unwrap().is_sign_negative());
        assert_eq!(stats.repaired_strings, 1);
        drop(out.clone());
        drop(out);
        drop(input);
    }
    #[test]
    fn array_tool_roots_are_repaired_before_row_acceptance() {
        let input = parse(r#"["\\u0041",{"__proto__":"\\u0042","good":"\ud800\\u0043"}]"#);
        let (out, stats) = repair_tool_input("Bash", &input);
        assert_eq!(out.string_at("/0"), Some("A"));
        assert_eq!(out.string_units("/1/good"), Some(vec![0xd800, 0x43]));
        assert!(out
            .at_pointer("/1")
            .unwrap()
            .get_property("__proto__")
            .is_none());
        assert_eq!(stats.repaired_strings, 2);
        assert_eq!(stats.proto_keys, 1);
    }
}
