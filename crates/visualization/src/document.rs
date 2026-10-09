//! Wrap an author fragment into the sandboxed content document, and build the
//! Content Security Policies the hosts send with the shell and the document.
//!
//! The stored revision is only the author's fragment; the runtime around it
//! is applied when the document is served, so a fix here reaches every
//! historical revision.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::assets;

/// Derived from upstream `visualize.html` by `build.rs`.
const RUNTIME_TEMPLATE: &str = include_str!(concat!(env!("OUT_DIR"), "/visualize.runtime.html"));
const FRAGMENT_PLACEHOLDER: &str = "<!--__INLINE_VISUALIZATION_FRAGMENT__-->";
const ASSET_BASE_PLACEHOLDER: &str = "{{ASSET_BASE}}";

/// Path prefix of every library asset under the host origin.
pub const ASSET_PATH: &str = "/asset/";
/// Path prefix of content documents under the host origin.
pub const DOC_PATH: &str = "/doc/";

/// Light or dark appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    /// Light appearance.
    #[default]
    Light,
    /// Dark appearance.
    Dark,
}

impl ThemeMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

/// Host theme: appearance plus values for the upstream CSS variables
/// (`--color-background-primary`, …) that `visualize.css` reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Theme {
    /// Appearance.
    pub mode: ThemeMode,
    /// Variable name → CSS value. Unknown names and unsafe values are dropped.
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
}

/// The host-variable names `visualize.css` consumes; nothing else may be set.
pub const THEME_TOKENS: &[&str] = &[
    "--color-background-primary",
    "--color-background-secondary",
    "--color-background-info",
    "--color-text-primary",
    "--color-text-secondary",
    "--color-text-info",
    "--color-text-inverse",
    "--color-text-warning",
    "--color-border-primary",
    "--color-border-secondary",
    "--color-ring-primary",
    "--font-text-md-size",
    "--border-radius-lg",
];

fn safe_token_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b" #%(),./-".contains(&b))
}

impl Theme {
    /// The sanitized `:root{...}` rule for this theme.
    #[must_use]
    pub fn css(&self) -> String {
        let mut css = String::from(":root{");
        for (name, value) in &self.tokens {
            if THEME_TOKENS.contains(&name.as_str()) && safe_token_value(value) {
                let _ = write!(css, "{name}:{value};");
            }
        }
        css.push('}');
        css
    }

    /// Theme as sent to the shell and the content runtime, sanitized.
    #[must_use]
    pub fn sanitized(&self) -> Self {
        Self {
            mode: self.mode,
            tokens: self
                .tokens
                .iter()
                .filter(|(name, value)| {
                    THEME_TOKENS.contains(&name.as_str()) && safe_token_value(value)
                })
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        }
    }
}

/// Confirmed widget state embedded into a freshly served document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootState {
    /// Monotonic version of the confirmed state; `0` before the first save.
    pub version: u64,
    /// State the widget shares with the model on "continue analysis".
    pub model_content: serde_json::Value,
    /// State that stays local to the widget.
    pub private_content: serde_json::Value,
}

impl Default for BootState {
    fn default() -> Self {
        Self {
            version: 0,
            model_content: serde_json::Value::Null,
            private_content: serde_json::Value::Null,
        }
    }
}

/// Everything the wrapper needs for one mount.
#[derive(Debug, Clone)]
pub struct DocumentParams<'a> {
    /// Host origin, e.g. `lingxi-viz://visualization`.
    pub origin: &'a str,
    /// Title shown to assistive technology.
    pub title: &'a str,
    /// The stored author fragment.
    pub fragment: &'a str,
    /// Theme at mount time.
    pub theme: &'a Theme,
    /// BCP-47 language tag for `<html lang>`.
    pub locale: &'a str,
    /// Mount generation the runtime stamps on every message.
    pub generation: u64,
    /// Confirmed state at mount time.
    pub state: &'a BootState,
    /// Whether the mount starts expanded.
    pub expanded: bool,
}

/// Validate a host origin: `scheme://host` with no path, port or userinfo.
#[must_use]
pub fn valid_origin(origin: &str) -> bool {
    let Some((scheme, host)) = origin.split_once("://") else {
        return false;
    };
    let scheme_ok = scheme.starts_with(|c: char| c.is_ascii_lowercase())
        && scheme
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"+.-".contains(&b));
    let host_ok = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-".contains(&b));
    scheme_ok && host_ok
}

fn asset_source(origin: &str) -> String {
    format!("{origin}{ASSET_PATH}")
}

fn content_directives(origin: &str) -> String {
    let asset = asset_source(origin);
    format!(
        "default-src 'none'; script-src 'unsafe-inline' 'unsafe-eval' 'wasm-unsafe-eval' {asset}; \
style-src 'unsafe-inline' {asset}; img-src data: blob: {asset}; font-src data: {asset}; \
media-src data: blob:; worker-src blob:; connect-src blob: data:; frame-src 'none'; \
object-src 'none'; base-uri 'none'; form-action 'none'"
    )
}

/// Response-header CSP for a content document. The `sandbox` directive keeps
/// the document in an opaque origin even if it is ever loaded outside the
/// shell's sandboxed frame.
#[must_use]
pub fn content_csp(origin: &str) -> String {
    format!("sandbox allow-scripts; {}", content_directives(origin))
}

/// `<meta>` fallback for the content document (`sandbox` is header-only).
#[must_use]
pub fn content_meta_csp(origin: &str) -> String {
    content_directives(origin)
}

/// Response-header CSP for the trusted shell page: no inline script at all.
#[must_use]
pub fn shell_csp(origin: &str) -> String {
    format!(
        "default-src 'none'; script-src {origin}/shell.js; style-src {origin}/shell.css; \
frame-src {origin}{DOC_PATH}; img-src 'none'; connect-src 'none'; base-uri 'none'; \
form-action 'none'; frame-ancestors 'none'"
    )
}

/// Escape text for an HTML text node or a double-quoted attribute.
#[must_use]
pub fn escape_html(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for character in input.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// JSON safe to embed inside a `<script>` element.
#[must_use]
pub fn script_json(value: &serde_json::Value) -> String {
    let json = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    let mut escaped = String::with_capacity(json.len());
    for character in json.chars() {
        match character {
            '<' => escaped.push_str("\\u003c"),
            '>' => escaped.push_str("\\u003e"),
            '&' => escaped.push_str("\\u0026"),
            '\u{2028}' => escaped.push_str("\\u2028"),
            '\u{2029}' => escaped.push_str("\\u2029"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn valid_locale(locale: &str) -> &str {
    let ok = !locale.is_empty()
        && locale.len() <= 35
        && locale
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if ok {
        locale
    } else {
        "en"
    }
}

/// Whether the fragment refers to the bundled D3 global.
fn uses_d3(fragment: &str) -> bool {
    let bytes = fragment.as_bytes();
    fragment.match_indices("d3").any(|(at, _)| {
        let before = at.checked_sub(1).map(|i| bytes[i]);
        let after = bytes.get(at + 2).copied();
        let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
        !before.is_some_and(ident) && !after.is_some_and(ident)
    })
}

/// The complete content document for one mount.
#[must_use]
pub fn render_content_document(params: &DocumentParams<'_>) -> String {
    let asset_base = asset_source(params.origin);
    let runtime = RUNTIME_TEMPLATE
        .replace(ASSET_BASE_PLACEHOLDER, &asset_base)
        .replacen(FRAGMENT_PLACEHOLDER, params.fragment, 1);
    let theme = params.theme.sanitized();
    let boot = serde_json::json!({
        "generation": params.generation,
        "state": params.state,
        "theme": theme,
        "locale": valid_locale(params.locale),
        "expanded": params.expanded,
    });
    let d3 = if uses_d3(params.fragment) {
        format!("<script src=\"{asset_base}{}\"></script>", assets::D3)
    } else {
        String::new()
    };
    format!(
        "<!doctype html><html lang=\"{lang}\" data-theme=\"{mode}\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"referrer\" content=\"no-referrer\">\
<meta http-equiv=\"Content-Security-Policy\" content=\"{csp}\">\
<title>{title}</title><style>{css}\nhtml>body{{padding:0}}</style>\
<style id=\"lingxi-theme\">{theme_css}</style>\
<script>window.__lingxiVisualizationBoot={boot};</script>\
<script>{content_runtime}</script>{d3}</head><body>{runtime}</body></html>",
        lang = escape_html(valid_locale(params.locale)),
        mode = theme.mode.as_str(),
        csp = content_meta_csp(params.origin),
        title = escape_html(params.title),
        css = assets::VISUALIZE_CSS,
        theme_css = theme.css(),
        boot = script_json(&boot),
        content_runtime = assets::CONTENT_RUNTIME_JS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: &str = "lingxi-viz://visualization";

    fn document(fragment: &str) -> String {
        render_content_document(&DocumentParams {
            origin: ORIGIN,
            title: "Q3 <sales>",
            fragment,
            theme: &Theme {
                mode: ThemeMode::Dark,
                tokens: BTreeMap::from([
                    (
                        "--color-background-primary".to_string(),
                        "#101010".to_string(),
                    ),
                    (
                        "--color-text-primary".to_string(),
                        "red;}</style><script>".to_string(),
                    ),
                    ("--evil".to_string(), "#fff".to_string()),
                ]),
            },
            locale: "zh-CN",
            generation: 7,
            state: &BootState::default(),
            expanded: false,
        })
    }

    #[test]
    fn document_contract_matches_upstream_runtime_without_external_urls() {
        let html = document("<div id=\"widget\"><canvas id=\"chart\"></canvas></div><script>globalThis.chartRendered = true;</script>");
        assert!(html.starts_with("<!doctype html><html lang=\"zh-CN\" data-theme=\"dark\">"));
        assert!(html.contains(".viz-controls"), "canonical control styles");
        assert!(html.contains("lingxi-viz://visualization/asset/floating-ui.dom.umd.min.js"));
        assert!(html.contains("lingxi-viz://visualization/asset/lucide.min.js"));
        assert!(html.contains("<canvas id=\"chart\"></canvas>"));
        assert!(html.contains("globalThis.chartRendered = true"));
        assert!(html.contains("Content-Security-Policy"));
        assert!(html.contains("<title>Q3 &lt;sales&gt;</title>"));
        assert!(!html.contains("unpkg.com"));
        for line in html.split(['"', '\'', ' ', '(', ')']) {
            if line.starts_with("http://") || line.starts_with("https://") {
                assert!(
                    line.starts_with("http://www.w3.org/"),
                    "external URL in document: {line}"
                );
            }
        }
        assert!(
            !html.contains(assets::D3),
            "D3 loads only for fragments that use it"
        );
    }

    #[test]
    fn theme_is_sanitized_and_inlined_before_first_paint() {
        let html = document("<p>x</p>");
        assert!(html.contains(
            "<style id=\"lingxi-theme\">:root{--color-background-primary:#101010;}</style>"
        ));
        assert!(!html.contains("--evil"));
        assert!(!html.contains("red;}</style><script>"));
    }

    #[test]
    fn boot_json_cannot_break_out_of_its_script() {
        let state = BootState {
            version: 3,
            model_content: serde_json::json!({"note": "</script><script>alert(1)</script>"}),
            private_content: serde_json::json!("\u{2028}"),
        };
        let html = render_content_document(&DocumentParams {
            origin: ORIGIN,
            title: "t",
            fragment: "",
            theme: &Theme::default(),
            locale: "\"><script>",
            generation: 1,
            state: &state,
            expanded: true,
        });
        let boot = html
            .split("window.__lingxiVisualizationBoot=")
            .nth(1)
            .and_then(|rest| rest.split(";</script>").next())
            .unwrap();
        assert!(!boot.contains('<'));
        assert!(boot.contains("\\u003c/script\\u003e"));
        assert!(boot.contains("\\u2028"));
        assert!(html.contains("lang=\"en\""), "invalid locale falls back");
    }

    #[test]
    fn d3_is_loaded_only_when_referenced() {
        assert!(document("<script>d3.select('#x')</script>").contains("/asset/d3.min.js"));
        assert!(
            !document("<script>const ad3x = 1; let d30 = 2</script>").contains("/asset/d3.min.js")
        );
    }

    #[test]
    fn content_csp_is_sandboxed_and_asset_scoped() {
        let csp = content_csp(ORIGIN);
        assert!(csp.starts_with("sandbox allow-scripts; "));
        assert!(!csp.contains("allow-same-origin"));
        assert!(csp.contains("script-src 'unsafe-inline' 'unsafe-eval' 'wasm-unsafe-eval' lingxi-viz://visualization/asset/;"));
        assert!(csp.contains("connect-src blob: data:;"));
        assert!(csp.contains("form-action 'none'"));
        assert!(!content_meta_csp(ORIGIN).contains("sandbox"));
        let shell = shell_csp(ORIGIN);
        assert!(shell.contains("script-src lingxi-viz://visualization/shell.js;"));
        assert!(!shell.contains("unsafe-inline"));
        assert!(shell.contains("frame-src lingxi-viz://visualization/doc/;"));
    }

    #[test]
    fn origins_are_validated() {
        assert!(valid_origin("lingxi-viz://visualization"));
        assert!(valid_origin("https://lingxi-visualization.invalid"));
        assert!(!valid_origin("https://example.com/path"));
        assert!(!valid_origin("javascript:alert(1)"));
        assert!(!valid_origin("lingxi-viz://visualization:8080"));
        assert!(!valid_origin("lingxi-viz://user@visualization"));
    }
}
