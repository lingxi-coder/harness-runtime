//! Shared system-reminder envelope escaping.
/// Escape closing envelope tags embedded in untrusted reminder content.
pub fn escape_closing_system_reminder(s: &str) -> String {
    /// JS `\s`.
    fn is_js_space(c: char) -> bool {
        c == '\u{feff}' || c.is_whitespace()
    }
    // Hand-rolled scanner: the crate has no regex dependency and the pattern is
    // small enough to match directly.
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '<' {
            let mut j = i + 1;
            while j < chars.len() && is_js_space(chars[j]) {
                j += 1;
            }
            if j < chars.len() && chars[j] == '/' {
                j += 1;
                while j < chars.len() && is_js_space(chars[j]) {
                    j += 1;
                }
                let tag: String = chars
                    .iter()
                    .skip(j)
                    .take("system-reminder".len())
                    .collect::<String>()
                    .to_ascii_lowercase();
                if tag == "system-reminder" {
                    j += "system-reminder".len();
                    while j < chars.len() && is_js_space(chars[j]) {
                        j += 1;
                    }
                    if j < chars.len() && chars[j] == '>' {
                        out.push_str("&lt;/system-reminder&gt;");
                        i = j + 1;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}
