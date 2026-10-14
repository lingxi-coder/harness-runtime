//! Deterministic publish-time checks on an app document.
//!
//! An app runs offline in a WebView at its own origin, with no native bridge,
//! Web Workers or device permissions. The checks reject what can never work
//! and warn about what breaks offline, so the agent gets a readable reason at
//! publish time instead of an app that silently fails.

use crate::store::MAX_DOCUMENT_BYTES;

/// Outcome of [`check_document`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentReport {
    /// Problems that block publishing.
    pub errors: Vec<String>,
    /// Problems the agent should fix but that do not block publishing.
    pub warnings: Vec<String>,
}

impl DocumentReport {
    /// True when publishing may proceed.
    #[must_use]
    pub fn is_publishable(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Attributes that make the page load a resource, as opposed to a link the
/// user follows (`<a href>` leaves the app after the user confirms).
const LOADING_ATTRIBUTES: &[&str] = &["src", "srcset", "poster", "data", "xlink:href"];
const NETWORK_CALLS: &[&str] = &["fetch(", "WebSocket(", "EventSource(", "sendBeacon("];
const WORKERS: &[&str] = &["new Worker(", "new SharedWorker(", "serviceWorker"];

/// Does `value` (just after an opening quote or `(`) name a network location?
fn is_external(value: &str) -> bool {
    let value = value
        .trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '"' || c == '\'' || c == '`');
    let lower: String = value
        .chars()
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    ["http:", "https:", "ws:", "wss:", "//"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b':'
}

/// Byte offsets where `needle` occurs as a whole word in lowercase `lower`.
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

/// Line number (1-based) of a byte offset, for messages.
fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|&b| b == b'\n').count() + 1
}

/// Where the document loads something from the network.
fn network_loads(html: &str, lower: &str) -> Vec<String> {
    let mut hits: Vec<String> = Vec::new();
    for attribute in LOADING_ATTRIBUTES {
        for at in word_hits(lower, attribute) {
            let rest = lower[at + attribute.len()..].trim_start();
            if let Some(value) = rest.strip_prefix('=') {
                if is_external(value) {
                    hits.push(format!("line {}: {attribute}=", line_of(html, at)));
                }
            }
        }
    }
    // A stylesheet, icon or preload `<link>` loads its href; an `<a href>` does not.
    for at in word_hits(lower, "<link") {
        let tag_end = lower[at..].find('>').map_or(lower.len(), |end| at + end);
        let tag = &lower[at..tag_end];
        if let Some(position) = tag.find("href=") {
            if is_external(&tag[position + 5..]) {
                hits.push(format!("line {}: <link href=", line_of(html, at)));
            }
        }
    }
    for at in lower.match_indices("url(").map(|(at, _)| at) {
        if is_external(&lower[at + 4..]) {
            hits.push(format!("line {}: url()", line_of(html, at)));
        }
    }
    for at in lower.match_indices("@import").map(|(at, _)| at) {
        let rest = lower[at + 7..].trim_start();
        let rest = rest.strip_prefix("url(").unwrap_or(rest);
        if is_external(rest) {
            hits.push(format!("line {}: @import", line_of(html, at)));
        }
    }
    for marker in ["import(", "from ", "import "] {
        for at in lower.match_indices(marker).map(|(at, _)| at) {
            let rest = lower[at + marker.len()..].trim_start();
            if rest.starts_with(['"', '\'', '`']) && is_external(rest) {
                hits.push(format!("line {}: module import", line_of(html, at)));
            }
        }
    }
    for call in NETWORK_CALLS {
        let needle = call.to_ascii_lowercase();
        for at in lower.match_indices(&needle).map(|(at, _)| at) {
            let rest = lower[at + needle.len()..].trim_start();
            if is_external(rest) {
                hits.push(format!("line {}: {call}", line_of(html, at)));
            }
        }
    }
    hits.sort();
    hits.dedup();
    hits
}

/// Check an app document for the rules every host enforces.
#[must_use]
pub fn check_document(html: &str) -> DocumentReport {
    let mut report = DocumentReport::default();
    if html.len() > MAX_DOCUMENT_BYTES {
        report.errors.push(format!(
            "The document is {} bytes; the limit is {MAX_DOCUMENT_BYTES} bytes (25 MiB).",
            html.len()
        ));
        return report;
    }
    if html.trim().is_empty() {
        report.errors.push("The document is empty.".to_string());
        return report;
    }
    if html.contains('\0') {
        report
            .errors
            .push("The document contains a NUL character.".to_string());
        return report;
    }
    let lower = html.to_ascii_lowercase();
    if !["!doctype html", "html", "head", "body"]
        .iter()
        .any(|tag| tag_present(&lower, tag))
    {
        report.errors.push(
            "Write a complete HTML document: start with <!doctype html>, then <html>, <head> and <body>."
                .to_string(),
        );
    }

    let loads = network_loads(html, &lower);
    if !loads.is_empty() {
        report.warnings.push(format!(
            "These load from the network, so the app breaks offline ({}). Inline the CSS, JavaScript and images instead.",
            loads.join(", ")
        ));
    }
    for worker in WORKERS {
        if html.contains(worker) {
            report.warnings.push(format!(
                "{} is blocked in apps; run the work on the page itself.",
                worker.trim_end_matches('(')
            ));
        }
    }
    for at in word_hits(&lower, "<form") {
        let tag_end = lower[at..].find('>').map_or(lower.len(), |end| at + end);
        let tag = &lower[at..tag_end];
        if tag.contains("action=") {
            report.warnings.push(format!(
                "line {}: a form that submits leaves the app. Drop action and handle the submit event in script.",
                line_of(html, at)
            ));
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>t</title></head>\n<body><p>x</p><script>localStorage.n = 1</script></body></html>";

    #[test]
    fn a_complete_offline_document_is_clean() {
        assert_eq!(check_document(PAGE), DocumentReport::default());
        assert!(check_document("<body>x</body>").is_publishable());
    }

    #[test]
    fn fragments_empty_nul_and_oversized_documents_are_rejected() {
        for html in ["<div>only a fragment</div>", "  \n", "<!doctype html>\0"] {
            assert!(!check_document(html).is_publishable(), "{html:?}");
        }
        assert!(!check_document(&"a".repeat(MAX_DOCUMENT_BYTES + 1)).is_publishable());
        assert!(!check_document("<header>x</header><bodyguard>").is_publishable());
    }

    #[test]
    fn network_loads_warn_but_links_and_data_urls_do_not() {
        for html in [
            "<!doctype html><script src='https://cdn.test/a.js'></script>",
            "<!doctype html><link rel=stylesheet href=\"//fonts.test/a.css\">",
            "<!doctype html><img srcset=\"https://x.test/a.png 2x\">",
            "<!doctype html><style>@import url('https://x.test/a.css');</style>",
            "<!doctype html><script>fetch('https://api.test/x')</script>",
            "<!doctype html><script type=module>import x from \"https://esm.test/x.js\"</script>",
        ] {
            let report = check_document(html);
            assert!(report.is_publishable(), "{html}");
            assert_eq!(report.warnings.len(), 1, "{html}: {:?}", report.warnings);
            assert!(report.warnings[0].contains("breaks offline"), "{html}");
        }
        for html in [
            "<!doctype html><a href=\"https://example.com\">site</a>",
            "<!doctype html><img src=\"data:image/png;base64,AAAA\">",
            "<!doctype html><p>See https://example.com</p>",
            "<!doctype html><svg xmlns=\"http://www.w3.org/2000/svg\"></svg>",
        ] {
            assert_eq!(check_document(html), DocumentReport::default(), "{html}");
        }
    }

    #[test]
    fn workers_and_submitting_forms_warn() {
        let report = check_document(
            "<!doctype html><form action=\"/x\"></form><script>new Worker('w.js'); navigator.serviceWorker</script>",
        );
        assert!(report.is_publishable());
        assert_eq!(report.warnings.len(), 3, "{:?}", report.warnings);
        assert!(check_document("<!doctype html><form id=f></form>")
            .warnings
            .is_empty());
    }

    #[test]
    fn warnings_point_at_lines() {
        let report = check_document(
            "<!doctype html>\n<p>x</p>\n<script src=\"https://x.test/a.js\"></script>",
        );
        assert!(
            report.warnings[0].contains("line 3: src="),
            "{:?}",
            report.warnings
        );
    }
}
