//! Repair double-escaped unicode in model-emitted tool inputs — claude-code
//! `jYd` / `L6s` (2.1.218), successor to 2.1.217's `sOo`.
//!
//! # What this fixes
//!
//! A model sometimes emits the TEXT `你` instead of the character `你` in a
//! tool argument (the JSON string contains a literal backslash-u sequence rather
//! than an encoded escape). Left alone, `Edit`'s `old_string` would never match
//! the file, `Bash` would run mojibake, and paths would not resolve. Claude Code
//! rewrites those literal sequences back into real characters before the tool
//! sees them, so this changes ACTUAL argument values — it is not telemetry.
//!
//! # The two guards that make it safe
//!
//! Blind unescaping would corrupt legitimate content, so the binary applies two
//! rules that this port reproduces exactly:
//!
//! * **Backslash parity** — an ODD number of backslashes immediately before the
//!   match means the `\u` is itself escaped (`\\u0041` is the literal text
//!   `A`), so it is left untouched.
//! * **Windows paths** (`Cky`, NEW in 2.1.218) — a drive-letter path (`C:\…`)
//!   or a UNC path (`\\host\share`) is returned verbatim and counted, because
//!   `\u` there is a path segment (`C:\users\…`), not an escape.
//!
//! Lone surrogates are also left as text; only a well-formed high+low pair is
//! joined into one character.

use serde_json::Value;

/// Counters reported by [`repair_tool_input`] — claude-code's `{repairedStrings,
/// windowsPathSkips}`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RepairStats {
    /// Strings whose content actually changed.
    pub repaired_strings: u32,
    /// Strings skipped because they look like a Windows path.
    pub windows_path_skips: u32,
}

impl RepairStats {
    /// `true` when either counter fired (the binary's telemetry condition).
    #[must_use]
    pub fn is_noteworthy(self) -> bool {
        self.repaired_strings > 0 || self.windows_path_skips > 0
    }
}

/// claude-code `qYd` / `vky` — a surrogate PAIR `\uD8xx\uDCxx` (groups 1+2) or a
/// single `\uXXXX` (group 3), matched against the LITERAL two-character `\u`.
fn escape_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"\\u([dD][89aAbB][0-9a-fA-F]{2})\\u([dD][c-fC-F][0-9a-fA-F]{2})|\\u([0-9a-fA-F]{4})",
        )
        .expect("static unicode-escape regex")
    })
}

/// claude-code `Cky` — does this string look like a Windows path, whose
/// backslashes must not be treated as escapes?
///
/// ```text
/// /(?:^|[^A-Za-z])[A-Za-z]:[\\/]|(?:^|[\s"'=])\\\\[^\s\\/]+[\\/](?!\\)/
/// ```
///
/// Hand-rolled because the second alternative uses a negative lookahead, which
/// the `regex` crate does not support. Differential-fuzzed against the real
/// regex under `node` (400k+ cases, 0 mismatches).
#[must_use]
pub fn looks_like_windows_path(s: &str) -> bool {
    // Scanned over CHARS, not bytes: JS `\s` is UNICODE, so a NBSP/U+2028/U+3000
    // before the `\\` is a valid boundary and inside a segment terminates it. A
    // byte scan with `is_ascii_whitespace` diverges in BOTH directions — it
    // under-skips (mangling a real UNC path's `\u`) and over-skips (suppressing a
    // legitimate repair).
    let c: Vec<char> = s.chars().collect();

    // (?:^|[^A-Za-z])[A-Za-z]:[\\/]  — a drive letter not preceded by a letter.
    for i in 0..c.len() {
        if i + 2 < c.len()
            && c[i].is_ascii_alphabetic()
            && c[i + 1] == ':'
            && (c[i + 2] == '\\' || c[i + 2] == '/')
            && (i == 0 || !c[i - 1].is_ascii_alphabetic())
        {
            return true;
        }
    }

    // (?:^|[\s"'=])\\\\[^\s\\/]+[\\/](?!\\)  — a UNC path `\\host\share`.
    for i in 0..c.len().saturating_sub(1) {
        if c[i] != '\\' || c[i + 1] != '\\' {
            continue;
        }
        let boundary = i == 0 || {
            let p = c[i - 1];
            is_js_whitespace(p) || p == '"' || p == '\'' || p == '='
        };
        if !boundary {
            continue;
        }
        // one or more chars that are not whitespace, `\` or `/`
        let mut j = i + 2;
        let start = j;
        while j < c.len() && !is_js_whitespace(c[j]) && c[j] != '\\' && c[j] != '/' {
            j += 1;
        }
        // …followed by a separator NOT followed by another backslash.
        if j > start && j < c.len() && (c[j] == '\\' || c[j] == '/') && c.get(j + 1) != Some(&'\\')
        {
            return true;
        }
    }
    false
}

/// ECMAScript `\s` — `WhiteSpace ∪ LineTerminator`. Deliberately NOT
/// `char::is_whitespace` (which excludes U+FEFF and includes U+0085), and
/// certainly not `is_ascii_whitespace`.
#[must_use]
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// Repair one string — the `typeof e === "string"` arm of `L6s`.
fn repair_str(s: &str, stats: &mut RepairStats) -> String {
    // Fast bail: `if(!e.includes("\\u")) return e;`
    if !s.contains("\\u") {
        return s.to_string();
    }
    let re = escape_re();
    // `if(!qYd.test(e)) return e;`
    if !re.is_match(s) {
        return s.to_string();
    }
    // `if(Cky.test(e)) return t.windowsPathSkips++, e;`
    if looks_like_windows_path(s) {
        stats.windows_path_skips += 1;
        return s.to_string();
    }

    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut last = 0usize;
    for caps in re.captures_iter(s) {
        let m = caps.get(0).expect("group 0 always present");
        let at = m.start();
        out.push_str(&s[last..at]);
        last = m.end();

        // Backslash parity: `let l=a; while(l>0&&e[l-1]==="\\")l--; if(a-l&1) return n;`
        let mut l = at;
        while l > 0 && bytes[l - 1] == b'\\' {
            l -= 1;
        }
        if (at - l) & 1 == 1 {
            out.push_str(m.as_str());
            continue;
        }

        if let (Some(hi), Some(lo)) = (caps.get(1), caps.get(2)) {
            // `String.fromCharCode(parseInt(o,16), parseInt(i,16))` — a pair.
            let hi = u16::from_str_radix(hi.as_str(), 16).unwrap_or(0);
            let lo = u16::from_str_radix(lo.as_str(), 16).unwrap_or(0);
            match String::from_utf16(&[hi, lo]) {
                Ok(decoded) => out.push_str(&decoded),
                Err(_) => out.push_str(m.as_str()),
            }
            continue;
        }

        let single = caps
            .get(3)
            .map_or(0u32, |g| u32::from_str_radix(g.as_str(), 16).unwrap_or(0));
        // `if(c>=55296&&c<=57343) return n;` — a LONE surrogate stays literal.
        if (0xD800..=0xDFFF).contains(&single) {
            out.push_str(m.as_str());
            continue;
        }
        match char::from_u32(single) {
            Some(c) => out.push(c),
            None => out.push_str(m.as_str()),
        }
    }
    out.push_str(&s[last..]);

    if out != s {
        stats.repaired_strings += 1;
    }
    out
}

/// claude-code `L6s` — recursively repair every string in a value.
#[must_use]
pub fn repair_value(v: &Value, stats: &mut RepairStats) -> Value {
    match v {
        Value::String(s) => Value::String(repair_str(s, stats)),
        Value::Array(items) => Value::Array(items.iter().map(|i| repair_value(i, stats)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, val)| (k.clone(), repair_value(val, stats)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The tool whose `script` argument is restored VERBATIM after repair — a
/// workflow script is source code, where `\uXXXX` is a real escape the author
/// wrote and must survive untouched.
const WORKFLOW_TOOL_NAME: &str = "Workflow";
/// The verbatim-restored field on [`WORKFLOW_TOOL_NAME`].
const WORKFLOW_SCRIPT_FIELD: &str = "script";

/// claude-code `jYd` — repair a tool_use input and report the counters.
///
/// Applied to EVERY assistant tool_use, with `Workflow.script` restored verbatim.
#[must_use]
pub fn repair_tool_input(tool_name: &str, input: &Value) -> (Value, RepairStats) {
    // Oracle `Uun` guard: repair only a plain OBJECT input —
    // `typeof i==="object" && i!==null && !dRt(i)`. `dRt` flags only the special
    // parse-failure marker `{[O7t]:{raw,len}}`, which this port never
    // constructs, so the guard reduces to "is a JSON object". A non-object input
    // (string/number/array/null) is passed through verbatim.
    if !input.is_object() {
        return (input.clone(), RepairStats::default());
    }
    let mut stats = RepairStats::default();
    let mut repaired = repair_value(input, &mut stats);

    // `if(s.name===pk && typeof a.script==="string") l.script=a.script` — the
    // restore is guarded on the field being a STRING; a non-string `script`
    // (array/object) is left REPAIRED like any other value.
    if tool_name == WORKFLOW_TOOL_NAME {
        if let Some(orig @ Value::String(_)) = input.get(WORKFLOW_SCRIPT_FIELD) {
            if let Some(obj) = repaired.as_object_mut() {
                obj.insert(WORKFLOW_SCRIPT_FIELD.to_string(), orig.clone());
            }
        }
    }

    if stats.is_noteworthy() {
        telemetry::emit_repair_double_escaped_unicode(
            stats.repaired_strings,
            stats.windows_path_skips,
        );
    }
    (repaired, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repair(s: &str) -> (String, RepairStats) {
        let mut st = RepairStats::default();
        (repair_str(s, &mut st), st)
    }

    #[test]
    fn rewrites_literal_escapes_into_characters() {
        // INPUT is the literal 12-character text `你好`, not the glyphs.
        let (out, st) = repair("hello \\u4f60\\u597d");
        assert_eq!(out, "hello \u{4f60}\u{597d}");
        assert_eq!(st.repaired_strings, 1);
        assert_eq!(st.windows_path_skips, 0);
    }

    #[test]
    fn joins_surrogate_pairs() {
        // U+1F600 written as a literal high+low surrogate pair.
        let (out, _) = repair("\\ud83d\\ude00");
        assert_eq!(out, "\u{1f600}");
    }

    #[test]
    fn leaves_lone_surrogates_alone() {
        let (out, st) = repair("\\ud83d alone");
        assert_eq!(out, "\\ud83d alone", "a lone surrogate stays literal");
        assert_eq!(st.repaired_strings, 0);
    }

    #[test]
    fn backslash_parity_protects_genuine_escapes() {
        // ZERO (even) preceding backslashes => a bare literal => repaired.
        let (out, st) = repair("\\u0041");
        assert_eq!(out, "A");
        assert_eq!(st.repaired_strings, 1);
        // ONE (odd) preceding backslash => the `\u` is itself escaped => untouched.
        let (out, st) = repair("\\\\u0041");
        assert_eq!(
            out, "\\\\u0041",
            "an escaped backslash must not be unescaped"
        );
        assert_eq!(st.repaired_strings, 0);
    }

    #[test]
    fn windows_paths_are_skipped_and_counted() {
        for p in [
            "C:\\users\\u0041",
            "see C:/temp\\u0041",
            "\\\\server\\share\\u0041",
        ] {
            let (out, st) = repair(p);
            assert_eq!(out, p, "{p} must survive verbatim");
            assert_eq!(st.windows_path_skips, 1, "{p} must be counted");
            assert_eq!(st.repaired_strings, 0);
        }
    }

    #[test]
    fn non_windows_strings_are_not_mistaken_for_paths() {
        // `12:30` is not a drive letter (needs an ASCII LETTER before the colon).
        assert!(!looks_like_windows_path("at 12:30/x"));
        // A letter immediately before `C:` excludes it via `[^A-Za-z]`.
        assert!(!looks_like_windows_path("abcC:\\x"));
        assert!(looks_like_windows_path("C:\\x"));
    }

    #[test]
    fn recurses_through_arrays_and_objects() {
        let mut st = RepairStats::default();
        let v = json!({"a": ["\\u4f60", {"b": "\\u597d"}], "n": 1, "t": true});
        let out = repair_value(&v, &mut st);
        assert_eq!(
            out,
            json!({"a": ["\u{4f60}", {"b": "\u{597d}"}], "n": 1, "t": true})
        );
        assert_eq!(st.repaired_strings, 2);
    }

    #[test]
    fn workflow_script_is_restored_verbatim() {
        // A workflow script is SOURCE CODE: its `\uXXXX` is an escape the author
        // wrote and must survive the repair untouched.
        let input = json!({"script": "const s = '\\u4f60';", "name": "\\u4f60"});
        let (out, _) = repair_tool_input("Workflow", &input);
        assert_eq!(
            out["script"], input["script"],
            "Workflow.script must be restored verbatim"
        );
        assert_eq!(
            out["name"], "\u{4f60}",
            "other Workflow fields are still repaired"
        );
        // Any OTHER tool gets its `script` repaired like any normal field.
        let (out2, _) = repair_tool_input("Bash", &input);
        assert_eq!(out2["script"], "const s = '\u{4f60}';");
    }

    /// REGRESSION (adversarial fuzz vs node, defect #1): JS `\s` is UNICODE.
    /// A byte scan with `is_ascii_whitespace` diverged in BOTH directions.
    #[test]
    fn js_unicode_whitespace_is_honoured_around_unc_paths() {
        // NBSP before `\\host\share` IS a valid boundary ⇒ still a Windows path
        // ⇒ the `A` must survive verbatim (byte-scan version corrupted it).
        let nbsp_boundary = "\u{a0}\\\\host\\share\\u0041";
        let (out, st) = repair(nbsp_boundary);
        assert_eq!(out, nbsp_boundary, "NBSP is a JS \\s boundary");
        assert_eq!(st.windows_path_skips, 1);
        assert_eq!(st.repaired_strings, 0);

        // NBSP INSIDE the host segment terminates it ⇒ NOT a UNC path ⇒ repair.
        let nbsp_in_segment = "\\\\ho\u{a0}st\\share\\u0041";
        let (out, st) = repair(nbsp_in_segment);
        assert_eq!(out, "\\\\ho\u{a0}st\\shareA", "NBSP ends the segment");
        assert_eq!(st.repaired_strings, 1);
        assert_eq!(st.windows_path_skips, 0);

        // The full JS `\s` set behaves as a boundary…
        for ws in [
            '\u{b}', '\u{1680}', '\u{2000}', '\u{200a}', '\u{2028}', '\u{2029}', '\u{202f}',
            '\u{205f}', '\u{3000}', '\u{feff}',
        ] {
            let s = format!("{ws}\\\\host\\share\\u0041");
            assert!(
                looks_like_windows_path(&s),
                "U+{:04X} must count as JS whitespace",
                ws as u32
            );
        }
        // …while these are NOT JS `\s` and must NOT act as a boundary.
        for non_ws in ['\u{200b}', '\u{180e}', '\u{85}'] {
            let s = format!("{non_ws}\\\\host\\share\\u0041");
            assert!(
                !looks_like_windows_path(&s),
                "U+{:04X} is not JS whitespace",
                non_ws as u32
            );
        }
    }

    /// REGRESSION (defect #3): the oracle guards the Workflow restore on
    /// `typeof a.script === "string"`; a non-string `script` stays REPAIRED.
    #[test]
    fn workflow_restore_only_applies_to_a_string_script() {
        let arr = json!({"script": ["\\u0041"]});
        let (out, _) = repair_tool_input("Workflow", &arr);
        assert_eq!(
            out["script"],
            json!(["A"]),
            "a non-string script is repaired"
        );

        let obj = json!({"script": {"k": "\\u0041"}});
        let (out, _) = repair_tool_input("Workflow", &obj);
        assert_eq!(out["script"], json!({"k": "A"}));

        // Absent `script` still repairs other object fields.
        let (out, _) = repair_tool_input("Workflow", &json!({"other": "\\u0041"}));
        assert_eq!(out["other"], "A");
        // A NON-OBJECT input is passed through verbatim — the oracle `Uun` guard
        // (`typeof i==="object"`) never repairs a top-level string.
        let (out, _) = repair_tool_input("Workflow", &json!("\\u0041"));
        assert_eq!(out, json!("\\u0041"));
    }

    #[test]
    fn untouched_input_reports_no_stats() {
        let (out, st) = repair_tool_input("Bash", &json!({"command": "ls -la"}));
        assert_eq!(out, json!({"command": "ls -la"}));
        assert!(!st.is_noteworthy());
    }
}
