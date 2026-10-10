//! Exact field readers for tool inputs carried as flat JavaScript JSON projections.

use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};

/// Read a string only when every original UTF-16 unit has an exact Rust UTF-8
/// representation. Callers that need a string for shell/path matching fail
/// closed when the source contains an isolated surrogate.
pub(crate) fn exact_string<'a>(value: JsonValueView<'a>) -> Option<&'a str> {
    let units = value.code_units()?;
    if std::char::decode_utf16(units.iter().copied()).any(|decoded| decoded.is_err()) {
        return None;
    }
    value.as_str()
}

/// Read one named string field without replacing unpaired UTF-16 units.
pub(crate) fn exact_string_field<'a>(
    input: &'a Utf16JsonProjection,
    field: &str,
) -> Option<&'a str> {
    exact_string(input.get_property(field)?)
}

/// Return an exact root-string projection of a selected source string after
/// ECMAScript trimming. The display view uses replacement characters only for
/// UI/log readers; `string_units` remains authoritative.
pub(crate) fn trimmed_string_field_projection(
    input: &Utf16JsonProjection,
    field: &str,
) -> Option<Utf16JsonProjection> {
    let units = trim_js_units(input.get_property(field)?.code_units()?).to_vec();
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units).ok()
}

/// Match JavaScript primitive stringification for a source value used by a
/// permission rule's generic field matcher. Strings with isolated surrogates
/// have no exact Rust string form and therefore cannot match a string rule.
pub(crate) fn stringify_primitive(value: JsonValueView<'_>) -> Option<String> {
    if let Some(text) = exact_string(value) {
        return Some(text.to_owned());
    }
    if let Some(number) = value.as_number() {
        return Some(number.to_string());
    }
    if let Some(boolean) = value.as_bool() {
        return Some(boolean.to_string());
    }
    None
}

/// Match JavaScript primitive stringification without requiring a Rust `str`.
/// Source strings retain every UTF-16 code unit; numbers and booleans use the
/// same generated UTF-8 spelling as [`stringify_primitive`].
pub(crate) fn stringify_primitive_units(value: JsonValueView<'_>) -> Option<Vec<u16>> {
    if let Some(units) = value.code_units() {
        return Some(units.to_vec());
    }
    if let Some(number) = value.as_number() {
        return Some(number.to_string().encode_utf16().collect());
    }
    value
        .as_bool()
        .map(|boolean| boolean.to_string().encode_utf16().collect())
}

/// Trim the ECMAScript `String.prototype.trim` whitespace set from exact
/// UTF-16 units. Every code point in this set is a single BMP unit.
pub(crate) fn trim_js_units(mut units: &[u16]) -> &[u16] {
    while units.first().is_some_and(|unit| is_js_trim_unit(*unit)) {
        units = &units[1..];
    }
    while units.last().is_some_and(|unit| is_js_trim_unit(*unit)) {
        units = &units[..units.len() - 1];
    }
    units
}

/// Match `ignore::gitignore::GitignoreBuilder::add_line` trailing-space
/// handling on Source units. Trailing JavaScript whitespace is removed, but a
/// final ASCII space preceded by an odd backslash run remains literal; this is
/// checked after trimming later whitespace so `\\ \uFEFF` keeps the escaped
/// space just as the builder does after its own right-trim pass.
pub(crate) fn trim_gitignore_end_units(units: &[u16]) -> &[u16] {
    let mut end = units.len();
    while end > 0 && is_js_trim_unit(units[end - 1]) {
        if units[end - 1] == b' ' as u16 {
            let mut backslashes = 0usize;
            let mut index = end - 1;
            while index > 0 && units[index - 1] == b'\\' as u16 {
                backslashes += 1;
                index -= 1;
            }
            if !backslashes.is_multiple_of(2) {
                break;
            }
        }
        end -= 1;
    }
    &units[..end]
}

fn is_js_trim_unit(unit: u16) -> bool {
    matches!(
        unit,
        0x0009..=0x000d
            | 0x0020
            | 0x00a0
            | 0x1680
            | 0x2000..=0x200a
            | 0x2028..=0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    )
}

pub(crate) fn is_js_trim_char(ch: char) -> bool {
    let mut units = [0u16; 2];
    let encoded = ch.encode_utf16(&mut units);
    encoded.len() == 1 && is_js_trim_unit(encoded[0])
}

/// Anchored, dot-all `*` glob matching over JavaScript UTF-16 code units.
/// This is the `_pi` matcher shape used for generic permission fields; `*`
/// matches any unit and every other unit is literal.
pub(crate) fn glob_matches_units(pattern: &[u16], value: &[u16]) -> bool {
    let (mut pattern_index, mut value_index) = (0usize, 0usize);
    let mut last_star = None;
    let mut value_after_star = 0usize;

    while value_index < value.len() {
        if pattern.get(pattern_index) == Some(&(b'*' as u16)) {
            last_star = Some(pattern_index);
            pattern_index += 1;
            value_after_star = value_index;
        } else if pattern.get(pattern_index) == Some(&value[value_index]) {
            pattern_index += 1;
            value_index += 1;
        } else if let Some(star_index) = last_star {
            value_after_star += 1;
            value_index = value_after_star;
            pattern_index = star_index + 1;
        } else {
            return false;
        }
    }

    pattern[pattern_index..]
        .iter()
        .all(|unit| *unit == b'*' as u16)
}

/// Map each UTF-16 code unit to one Rust scalar for path-glob operations.
/// ASCII stays ASCII so `/`, `*`, `?`, character-class punctuation and root
/// prefixes remain syntax. Every non-ASCII unit maps monotonically into a
/// private-use range: `u -> U+F0000 + (u - 0x80)`. The temporary candidate
/// alphabet contains only those ASCII scalars and marker scalars, so a glob
/// character-class range retains membership even across the gap between them:
/// every mapped candidate at or below an endpoint corresponds to an original
/// code unit at or below that endpoint. A surrogate pair maps to two scalars,
/// so `?` consumes two units just as a non-`u` JavaScript regex does. This
/// operation string is only for in-memory matching, never OS, model or wire
/// output.
pub(crate) fn exact_operation_strings(groups: &[&[u16]]) -> Option<Vec<String>> {
    groups
        .iter()
        .map(|units| {
            let mut output = String::new();
            for unit in units.iter().copied() {
                let scalar = if unit <= 0x7f {
                    u32::from(unit)
                } else {
                    0xF0000u32.checked_add(u32::from(unit) - 0x80)?
                };
                output.push(char::from_u32(scalar)?);
            }
            Some(output)
        })
        .collect()
}

/// Shell-rule helpers call Rust `trim()` at several intermediate wrapper and
/// comment-removal boundaries. Preserve that behavior in their temporary
/// operation strings while keeping the source NEL distinct from ECMAScript
/// whitespace. Shell matching escapes regex character classes, so this
/// whitespace-aware alphabet has no range-class ordering dependency.
pub(crate) fn exact_shell_operation_strings(groups: &[&[u16]]) -> Option<Vec<String>> {
    groups
        .iter()
        .map(|units| {
            let mut output = String::new();
            for unit in units.iter().copied() {
                match unit {
                    0x00..=0x7f => output.push(char::from_u32(u32::from(unit))?),
                    0xfeff => output.push('\u{0085}'),
                    _ if is_js_trim_unit(unit)
                        && char::from_u32(u32::from(unit)).is_some_and(|ch| ch.is_whitespace()) =>
                    {
                        output.push(char::from_u32(u32::from(unit))?);
                    }
                    _ => {
                        let scalar = 0xF0000u32.checked_add(u32::from(unit) - 0x80)?;
                        output.push(char::from_u32(scalar)?);
                    }
                }
            }
            Some(output)
        })
        .collect()
}

/// Construct a root-string projection for locally generated text.
pub(crate) fn generated_text(text: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(text.to_owned(), text.encode_utf16().collect())
        .expect("UTF-8 text has matching UTF-16 units")
}

/// Compare a source root string with host-authored text by its exact UTF-16
/// units. This is for semantic checks, never a display fallback.
pub(crate) fn equals_text(value: &Utf16JsonProjection, text: &str) -> bool {
    value
        .string_units("")
        .is_some_and(|units| units.as_slice().iter().copied().eq(text.encode_utf16()))
}

/// Test an exact source root-string prefix without decoding through UTF-8.
pub(crate) fn starts_with_text(value: &Utf16JsonProjection, prefix: &str) -> bool {
    let Some(units) = value.string_units("") else {
        return false;
    };
    let prefix = prefix.encode_utf16().collect::<Vec<_>>();
    units.starts_with(&prefix)
}

/// Search for host-authored text inside a source root string by UTF-16 units.
pub(crate) fn contains_text(value: &Utf16JsonProjection, needle: &str) -> bool {
    let Some(units) = value.string_units("") else {
        return false;
    };
    let needle = needle.encode_utf16().collect::<Vec<_>>();
    if needle.is_empty() {
        return true;
    }
    units
        .windows(needle.len())
        .any(|window| window == needle.as_slice())
}

/// Build one model/UI text field from generated UTF-8 fragments and exact
/// source-string projections. Source units remain authoritative.
pub(crate) fn joined_text(parts: &[TextPart<'_>]) -> Utf16JsonProjection {
    let mut units = Vec::new();
    for part in parts {
        match part {
            TextPart::Generated(text) => units.extend(text.encode_utf16()),
            TextPart::Source(text) => units.extend(
                text.string_units("")
                    .expect("source text fragment must be a root string"),
            ),
        }
    }
    let display = String::from_utf16_lossy(&units);
    Utf16JsonProjection::root_string(display, units)
        .expect("display is the lossless-or-replacement projection of UTF-16 units")
}

pub(crate) enum TextPart<'a> {
    Generated(&'a str),
    Source(&'a Utf16JsonProjection),
}

/// Preserve a projection while replacing one locally generated string field.
pub(crate) fn with_generated_string_field(
    input: &Utf16JsonProjection,
    field: &str,
    text: &str,
) -> Option<Utf16JsonProjection> {
    let mut output = input.clone();
    output.set_field(field, generated_text(text)).ok()?;
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_text_predicates_match_units_without_replacing_surrogates() {
        let value = Utf16JsonProjection::parse(r#""prefix-\ud800-tail""#).unwrap();
        assert!(starts_with_text(&value, "prefix-"));
        assert!(contains_text(&value, "-tail"));
        assert!(!equals_text(&value, "prefix-�-tail"));
    }

    #[test]
    fn javascript_glob_and_trim_keep_isolated_surrogates_as_units() {
        let pattern = [b'*' as u16, 0xd800, b'*' as u16];
        let value = [b' ' as u16, b'x' as u16, 0xd800, b'y' as u16, b' ' as u16];
        assert!(glob_matches_units(&pattern, trim_js_units(&value)));
        assert!(!glob_matches_units(
            &pattern,
            &[b'x' as u16, 0xd801, b'y' as u16]
        ));
    }

    #[test]
    fn path_operation_map_is_ordered_and_one_scalar_per_source_unit() {
        let ascii = [0x7fu16];
        let first_non_ascii = [0x80u16];
        let next_non_ascii = [0x81u16];
        let bmp_private_use = [0xe000u16];
        let isolated_surrogate = [0xd800u16];
        let supplementary_private_use = [0xdb80u16, 0xdc00u16]; // U+F0000
        let mapped = exact_operation_strings(&[
            &ascii,
            &first_non_ascii,
            &next_non_ascii,
            &bmp_private_use,
            &isolated_surrogate,
            &supplementary_private_use,
        ])
        .expect("every UTF-16 code unit has a valid scalar mapping");
        let scalar = |value: &str| value.chars().next().unwrap() as u32;
        assert!(scalar(&mapped[0]) < scalar(&mapped[1]));
        assert!(scalar(&mapped[1]) < scalar(&mapped[2]));
        assert!(scalar(&mapped[2]) < scalar(&mapped[3]));
        assert_ne!(mapped[3], mapped[4]);
        assert_eq!(mapped[4].chars().count(), 1);
        assert_eq!(mapped[5].chars().count(), 2);
        assert_ne!(mapped[5], char::from_u32(0xf0000).unwrap().to_string());

        let all_units = (0..=u16::MAX).collect::<Vec<_>>();
        let mapped_all = exact_operation_strings(&[&all_units]).unwrap();
        let mut scalars = mapped_all[0].chars().map(u32::from);
        let mut previous = scalars.next().unwrap();
        for scalar in scalars {
            assert!(
                previous < scalar,
                "mapping must preserve every code-unit order"
            );
            previous = scalar;
        }
        assert_eq!(mapped_all[0].chars().count(), usize::from(u16::MAX) + 1);
    }

    #[test]
    fn source_trim_distinguishes_feff_from_nel_and_shell_map_keeps_rust_trim() {
        let feff = [0xfeffu16];
        let nel = [0x0085u16];
        assert!(trim_js_units(&feff).is_empty());
        assert_eq!(trim_js_units(&nel), &nel);

        let path_map = exact_operation_strings(&[&feff, &nel]).unwrap();
        assert!(path_map[0].chars().next().unwrap() > path_map[1].chars().next().unwrap());
        let shell_map = exact_shell_operation_strings(&[&feff, &nel]).unwrap();
        assert!(shell_map[0].trim().is_empty());
        assert!(!shell_map[1].trim().is_empty());
    }

    #[test]
    fn gitignore_source_trim_preserves_escaped_ascii_space_after_feff() {
        let escaped_then_feff = [b'\\' as u16, b' ' as u16, 0xfeff];
        let even_backslash = [b'\\' as u16, b'\\' as u16, b' ' as u16];
        let nel = [0x0085u16];
        assert_eq!(
            trim_gitignore_end_units(&escaped_then_feff),
            &escaped_then_feff[..2]
        );
        assert_eq!(
            trim_gitignore_end_units(&even_backslash),
            &even_backslash[..2]
        );
        assert_eq!(trim_gitignore_end_units(&nel), &nel);
    }
}
