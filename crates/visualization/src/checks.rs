//! Deterministic publish-time checks on an author fragment.
//!
//! The sandbox and CSP already block every one of these at render time; the
//! checks exist so the agent gets a readable reason at publish time instead of
//! a widget that silently fails, and so all three platforms reject the same
//! fragments.

/// Largest fragment accepted, matching upstream `MAX_FRAGMENT_BYTES`.
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
const STORAGE_APIS: &[&str] = &[
    "localStorage",
    "sessionStorage",
    "indexedDB",
    "document.cookie",
];
const DIALOGS: &[&str] = &["alert(", "confirm(", "prompt("];

/// Does `value` (just after an opening quote or `(`) name a network location?
fn is_external(value: &str) -> bool {
    let value = value
        .trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '"' || c == '\'' || c == '`');
    let lower: String = value
        .chars()
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    ["http:", "https:", "ws:", "wss:", "ftp:", "file:", "//"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b':'
}

/// Byte offsets where `needle` occurs case-insensitively as a whole word.
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

    let mut external: Vec<String> = Vec::new();
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
            "Remove external URLs ({}). Visualizations run offline: inline CSS, JS and data, and use the bundled d3, lucide and Floating UI globals.",
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

    for api in STORAGE_APIS {
        if fragment.contains(api) {
            report.warnings.push(format!(
                "{api} throws inside the sandbox; keep widget state with the lingxi.saveState API instead."
            ));
        }
    }
    for dialog in DIALOGS {
        let bytes = fragment.as_bytes();
        let hit = fragment.match_indices(dialog).any(|(at, _)| {
            at == 0
                || !(bytes[at - 1].is_ascii_alphanumeric()
                    || bytes[at - 1] == b'_'
                    || bytes[at - 1] == b'.')
        });
        if hit {
            report.warnings.push(format!(
                "{}) is blocked inside the sandbox; render messages in the page instead.",
                dialog
            ));
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_fragment_is_publishable() {
        let report = check_fragment(
            "<div id=\"widget\"><svg xmlns=\"http://www.w3.org/2000/svg\"></svg></div><script>d3.select('#widget')</script>",
        );
        assert_eq!(report, FragmentReport::default());
    }

    #[test]
    fn document_shell_is_rejected() {
        let report = check_fragment("<!DOCTYPE html><HTML><head></head><body>x</body></html>");
        assert_eq!(report.errors.len(), 4);
        assert!(check_fragment("<header>ok</header><bodyguard>").is_publishable());
    }

    #[test]
    fn external_urls_in_loading_contexts_are_rejected() {
        for fragment in [
            "<img src=\"https://x.test/a.png\">",
            "<script src='//cdn.test/a.js'></script>",
            "<a href=\"http://x.test\">x</a>",
            "<div style=\"background:url( 'https://x.test/a.png')\"></div>",
            "<style>@import url(\"https://x.test/a.css\");</style>",
            "<style>@import 'https://x.test/a.css';</style>",
            "<script type=module>import x from \"https://esm.test/x.js\"</script>",
            "<script>await import('https://esm.test/x.js')</script>",
            "<script>fetch(\"https://api.test\")</script>",
            "<script>new WebSocket('wss://x.test')</script>",
            "<img SRCSET=\"https://x.test/a.png 2x\">",
        ] {
            let report = check_fragment(fragment);
            assert!(!report.is_publishable(), "{fragment}");
            assert!(report.errors[0].contains("external URLs"), "{fragment}");
        }
    }

    #[test]
    fn urls_in_prose_and_data_urls_are_fine() {
        assert!(check_fragment("<p>See https://example.com for details</p>").is_publishable());
        assert!(check_fragment("<img src=\"data:image/png;base64,AAAA\">").is_publishable());
        assert!(
            check_fragment("<script>const dataset = 1; fetch(blobUrl)</script>").is_publishable()
        );
    }

    #[test]
    fn submitting_forms_are_rejected_but_plain_forms_pass() {
        assert!(!check_fragment("<form action=\"/x\"><input></form>").is_publishable());
        assert!(!check_fragment("<form method=post>").is_publishable());
        assert!(check_fragment("<form id=f><input></form>").is_publishable());
    }

    #[test]
    fn storage_and_dialogs_warn() {
        let report =
            check_fragment("<script>localStorage.x=1; alert('hi'); obj.confirm(1)</script>");
        assert!(report.is_publishable());
        assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
    }

    #[test]
    fn oversized_and_empty_fragments_are_rejected() {
        assert!(!check_fragment(&"a".repeat(MAX_FRAGMENT_BYTES + 1)).is_publishable());
        assert!(!check_fragment("  \n").is_publishable());
    }

    #[test]
    fn errors_point_at_lines() {
        let report = check_fragment("<div>\n<img src=\"https://x.test/a.png\">\n</div>");
        assert!(
            report.errors[0].contains("line 2: src="),
            "{:?}",
            report.errors
        );
    }
}
