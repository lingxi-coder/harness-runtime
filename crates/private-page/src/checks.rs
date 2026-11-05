//! Publish-time checks on a private-page fragment.
//!
//! The document shell and the host WebView already refuse network loads. These
//! checks give the agent a readable reason before the page is stored.

/// Largest fragment accepted.
pub const MAX_FRAGMENT_BYTES: usize = 2 * 1024 * 1024;

/// Outcome of [`check_fragment`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FragmentReport {
    /// Problems that block publishing.
    pub errors: Vec<String>,
    /// Problems the agent should fix but that do not block publishing.
    pub warnings: Vec<String>,
}

impl FragmentReport {
    /// True when publishing may proceed.
    #[must_use]
    pub fn is_publishable(&self) -> bool {
        self.errors.is_empty()
    }
}

const URL_ATTRIBUTES: &[&str] = &[
    "src",
    "href",
    "srcset",
    "action",
    "formaction",
    "poster",
    "data",
    "xlink:href",
];
const NETWORK_CALLS: &[&str] = &[
    "fetch(",
    "WebSocket(",
    "EventSource(",
    "sendBeacon(",
    "importScripts(",
];

fn is_external(value: &str) -> bool {
    let value = value
        .trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '"' || c == '\'' || c == '`');
    let lower: String = value
        .chars()
        .take(12)
        .collect::<String>()
        .to_ascii_lowercase();
    [
        "http:", "https:", "ws:", "wss:", "ftp:", "file:", "javascript:", "//",
    ]
    .iter()
    .any(|scheme| lower.starts_with(scheme))
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b':'
}

fn word_hits<'a>(lower: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    let bytes = lower.as_bytes();
    lower.match_indices(needle).filter_map(move |(at, _)| {
        let before_ok = at == 0 || !is_word_byte(bytes[at - 1]);
        before_ok.then_some(at)
    })
}

fn tag_present(lower: &str, tag: &str) -> bool {
    let open = format!("<{tag}");
    let bytes = lower.as_bytes();
    lower.match_indices(&open).any(|(at, _)| {
        bytes
            .get(at + open.len())
            .is_none_or(|&next| next == b'>' || next == b'/' || next.is_ascii_whitespace())
    })
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|&b| b == b'\n').count() + 1
}

/// Check a fragment for the rules every host enforces.
#[must_use]
pub fn check_fragment(fragment: &str) -> FragmentReport {
    let mut report = FragmentReport::default();
    if fragment.len() > MAX_FRAGMENT_BYTES {
        report.errors.push(format!(
            "The fragment is {} bytes; the limit is {MAX_FRAGMENT_BYTES} bytes (2 MiB).",
            fragment.len()
        ));
        return report;
    }
    if fragment.trim().is_empty() {
        report.errors.push("The fragment is empty.".to_string());
        return report;
    }
    let lower = fragment.to_ascii_lowercase();
    for tag in ["!doctype", "html", "head", "body"] {
        if tag_present(&lower, tag) {
            report.errors.push(format!(
                "Remove the <{tag}> element: write only the fragment; {} wraps it in the document itself.",
                branding::PRODUCT_NAME
            ));
        }
    }

    let mut external = Vec::new();
    for attribute in URL_ATTRIBUTES {
        for at in word_hits(&lower, attribute) {
            let rest = lower[at + attribute.len()..].trim_start();
            if let Some(value) = rest.strip_prefix('=') {
                if is_external(value) {
                    external.push(format!("line {}: {attribute}=", line_of(fragment, at)));
                }
            }
        }
    }
    for at in lower.match_indices("url(").map(|(at, _)| at) {
        if is_external(&lower[at + 4..]) {
            external.push(format!("line {}: url()", line_of(fragment, at)));
        }
    }
    for at in lower.match_indices("@import").map(|(at, _)| at) {
        let rest = lower[at + 7..].trim_start();
        let rest = rest.strip_prefix("url(").unwrap_or(rest);
        if is_external(rest) {
            external.push(format!("line {}: @import", line_of(fragment, at)));
        }
    }
    for marker in ["import(", "from ", "import "] {
        for at in lower.match_indices(marker).map(|(at, _)| at) {
            let rest = lower[at + marker.len()..].trim_start();
            if rest.starts_with(['"', '\'', '`']) && is_external(rest) {
                external.push(format!("line {}: module import", line_of(fragment, at)));
            }
        }
    }
    for call in NETWORK_CALLS {
        let needle = call.to_ascii_lowercase();
        for at in lower.match_indices(&needle).map(|(at, _)| at) {
            let rest = lower[at + needle.len()..].trim_start();
            if is_external(rest) {
                external.push(format!("line {}: {call}", line_of(fragment, at)));
            }
        }
    }
    external.sort();
    external.dedup();
    if !external.is_empty() {
        report.errors.push(format!(
            "Remove external URLs ({}). Private pages run offline: inline CSS, JS and data.",
            external.join(", ")
        ));
    }
    for at in word_hits(&lower, "<form") {
        let tag_end = lower[at..].find('>').map_or(lower.len(), |end| at + end);
        let tag = &lower[at..tag_end];
        if tag.contains("action=") || tag.contains("method=") {
            report.errors.push(format!(
                "line {}: forms cannot submit. Drop action/method and handle the submit event in script.",
                line_of(fragment, at)
            ));
        }
    }
    for api in ["localStorage", "sessionStorage", "indexedDB", "document.cookie"] {
        if fragment.contains(api) {
            report.warnings.push(format!(
                "{api} throws inside the sandbox; keep state in the page itself."
            ));
        }
    }
    for dialog in ["alert(", "confirm(", "prompt("] {
        let bytes = fragment.as_bytes();
        let hit = fragment.match_indices(dialog).any(|(at, _)| {
            at == 0
                || !(bytes[at - 1].is_ascii_alphanumeric()
                    || bytes[at - 1] == b'_'
                    || bytes[at - 1] == b'.')
        });
        if hit {
            report.warnings.push(format!(
                "{dialog} is blocked inside the sandbox; render messages in the page instead."
            ));
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fragment_publishes_and_a_shell_or_network_url_does_not() {
        assert!(check_fragment("<p>See https://example.com</p>").is_publishable());
        assert!(!check_fragment("<!DOCTYPE html><body>x</body>").is_publishable());
        assert!(!check_fragment("<img src=\"https://x.test/a.png\">").is_publishable());
        assert!(!check_fragment("<a href=\"javascript:alert(1)\">x</a>").is_publishable());
    }
}
