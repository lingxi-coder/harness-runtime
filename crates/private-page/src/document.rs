//! Wrap an author fragment in the document shell served to LingXi.
//!
//! The stored revision is only the fragment. The shell is applied when the
//! page is opened, so a shell fix reaches every earlier revision.

/// Content Security Policy for a private page. Inline CSS and JS are the
/// page itself; nothing else may load.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'unsafe-inline'; \
style-src 'unsafe-inline'; img-src data: blob:; font-src data:; media-src data:; \
connect-src 'none'; form-action 'none'; base-uri 'none'; frame-ancestors 'none'";

/// The HTML document LingXi loads for `fragment`.
#[must_use]
pub fn wrap(title: &str, fragment: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n\
<meta http-equiv=\"Content-Security-Policy\" content=\"{csp}\">\n\
<meta name=\"color-scheme\" content=\"light dark\">\n<title>{title}</title>\n\
<style>html,body{{margin:0}}body{{font:16px/1.5 system-ui,sans-serif;padding:24px;background:Canvas;color:CanvasText}}</style>\n\
</head>\n<body>\n{fragment}\n</body>\n</html>\n",
        csp = CONTENT_SECURITY_POLICY,
        title = escape(title),
        fragment = fragment,
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_carries_the_fragment_and_escapes_the_title() {
        let html = wrap("A <b> & \"page\"", "<p>Hello</p>");
        assert!(html.contains("<p>Hello</p>"));
        assert!(html.contains("<title>A &lt;b&gt; &amp; &quot;page&quot;</title>"));
        assert!(html.contains(CONTENT_SECURITY_POLICY));
        assert!(!html.contains("http://"));
    }
}
