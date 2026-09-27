//! HTML→markdown conversion + the secondary-model prompt for the WebFetch apply
//! step. Ports `claude-code/src/tools/WebFetchTool/{utils.ts,prompt.ts}`.
// Under the default (feature-off) build these items are unused — the whole module
// is only exercised by `web_fetch`'s apply path under the `web-markdown` feature.
#![allow(dead_code)]

use crate::web_fetch::WEBFETCH_TRUNCATION_SUFFIX;
use once_cell::sync::Lazy;

/// Markdown is truncated to this many UTF-16 code units before the secondary
/// model, to avoid "Prompt is too long" errors. claude-code `utils.ts`
/// `MAX_MARKDOWN_LENGTH` (`Vnr`); the cap counts JS `String.length` units.
pub const MAX_MARKDOWN_LENGTH: usize = 100_000;

/// True when the `Content-Type` header denotes HTML (case-insensitive substring
/// match on `text/html`). Non-HTML bodies are used as-is (no conversion).
#[must_use]
pub fn is_html_content_type(content_type: &str) -> bool {
    content_type.to_ascii_lowercase().contains("text/html")
}

/// Truncate `markdown` to `MAX_MARKDOWN_LENGTH` UTF-16 code units, appending
/// [`WEBFETCH_TRUNCATION_SUFFIX`] when truncated. Byte-faithful port of the
/// `t.length > Vnr ? t.slice(0, Vnr) + suffix : t` expression in `utils.ts`,
/// where `.length`/`.slice` count UTF-16 code units (JS `String` semantics).
///
/// The cut lands on a char boundary: we take the longest prefix of whole
/// `char`s whose cumulative UTF-16 length is `<= MAX_MARKDOWN_LENGTH`. This
/// matches JS `slice(0, N)` for every input except the pathological case where
/// `N` bisects a surrogate pair (an astral char straddling the cap) — JS would
/// there emit a lone surrogate, which is unrepresentable in Rust's UTF-8
/// `String`, so we stop one astral char earlier. That boundary case can differ
/// by at most a single code point.
#[must_use]
pub fn truncate_markdown(markdown: String) -> String {
    if markdown.encode_utf16().count() <= MAX_MARKDOWN_LENGTH {
        return markdown;
    }
    let mut units = 0usize;
    let mut cut = markdown.len();
    for (byte_idx, ch) in markdown.char_indices() {
        if units + ch.len_utf16() > MAX_MARKDOWN_LENGTH {
            cut = byte_idx;
            break;
        }
        units += ch.len_utf16();
    }
    let mut out = markdown[..cut].to_string();
    out.push_str(WEBFETCH_TRUNCATION_SUFFIX);
    out
}

/// Convert HTML to markdown via `htmd` (the `turndown` analogue). On conversion
/// error, fall back to the original HTML. Only compiled under `web-markdown`.
#[cfg(feature = "web-markdown")]
#[must_use]
pub fn html_to_markdown(html: &str) -> String {
    htmd::convert(html).unwrap_or_else(|_| html.to_string())
}

/// Guidelines appended for a NON-preapproved domain (the strict default).
/// Byte-faithful to `prompt.ts`.
const GUIDELINES_STRICT: &str =
    "Provide a concise response based only on the content above. In your response:\n \
- Enforce a strict 125-character maximum for quotes from any source document. \
Open Source Software is ok as long as we respect the license.\n \
- Use quotation marks for exact language from articles; any language outside of \
the quotation should never be word-for-word the same.\n \
- You are not a lawyer and never comment on the legality of your own prompts and \
responses.\n - Never produce or reproduce exact song lyrics.";

/// Guidelines appended for a preapproved domain. Byte-faithful to `prompt.ts`.
const GUIDELINES_PREAPPROVED: &str =
    "Provide a concise response based on the content above. Include relevant \
details, code examples, and documentation excerpts as needed.";

/// Build the secondary-model prompt. Byte-faithful to `prompt.ts`
/// `makeSecondaryModelPrompt` (note the leading/trailing newlines).
#[must_use]
pub fn make_secondary_model_prompt(
    markdown_content: &str,
    prompt: &str,
    is_preapproved_domain: bool,
) -> String {
    let guidelines = if is_preapproved_domain {
        GUIDELINES_PREAPPROVED
    } else {
        GUIDELINES_STRICT
    };
    format!("\nWeb page content:\n---\n{markdown_content}\n---\n\n{prompt}\n\n{guidelines}\n")
}

// ---------------------------------------------------------------------------
// Preapproved domains (`WebFetchTool/preapproved.ts`)
// ---------------------------------------------------------------------------
//
// For legal and security concerns, WebFetch normally only accesses domains the
// user provided. The exception is a curated allowlist of code-related docs.
//
// SECURITY WARNING (verbatim from preapproved.ts): these preapproved domains are
// ONLY for WebFetch (GET requests). The sandbox network layer deliberately does
// NOT inherit this list — arbitrary network access (POST/uploads) to these hosts
// could enable data exfiltration. This port is consumed solely by WebFetch.
//
// `PREAPPROVED_HOSTS` is the FULL 1:1 data from preapproved.ts:14-131. Entries
// containing a "/" are path-scoped (e.g. "github.com/anthropics"); all others are
// hostname-only. The split mirrors the module-load IIFE (preapproved.ts:136-152):
// hostname-only entries go into a `HashSet` (O(1) lookup), and path-scoped entries
// become a per-host list of path prefixes.

/// Hostname-only preapproved entries — every `PREAPPROVED_HOST` with no "/".
/// 1:1 with `HOSTNAME_ONLY` (`preapproved.ts:136-152`). NOTE: `learn.microsoft.com`
/// appears twice in the TS source (Azure + C#/.NET); a Set dedups it — the Rust
/// `HashSet` does the same.
static HOSTNAME_ONLY: Lazy<std::collections::HashSet<&'static str>> = Lazy::new(|| {
    [
        // Anthropic
        "platform.claude.com",
        "code.claude.com",
        "modelcontextprotocol.io",
        "agentskills.io",
        // Top Programming Languages
        "docs.python.org",
        "en.cppreference.com",
        "docs.oracle.com",
        "learn.microsoft.com",
        "developer.mozilla.org",
        "go.dev",
        "pkg.go.dev",
        "www.php.net",
        "docs.swift.org",
        "kotlinlang.org",
        "ruby-doc.org",
        "doc.rust-lang.org",
        "www.typescriptlang.org",
        // Web & JavaScript Frameworks/Libraries
        "react.dev",
        "angular.io",
        "vuejs.org",
        "nextjs.org",
        "expressjs.com",
        "nodejs.org",
        "bun.sh",
        "jquery.com",
        "getbootstrap.com",
        "tailwindcss.com",
        "d3js.org",
        "threejs.org",
        "redux.js.org",
        "webpack.js.org",
        "jestjs.io",
        "reactrouter.com",
        // Python Frameworks & Libraries
        "docs.djangoproject.com",
        "flask.palletsprojects.com",
        "fastapi.tiangolo.com",
        "pandas.pydata.org",
        "numpy.org",
        "www.tensorflow.org",
        "pytorch.org",
        "scikit-learn.org",
        "matplotlib.org",
        "requests.readthedocs.io",
        "jupyter.org",
        // PHP Frameworks
        "laravel.com",
        "symfony.com",
        "wordpress.org",
        // Java Frameworks & Libraries
        "docs.spring.io",
        "hibernate.org",
        "tomcat.apache.org",
        "gradle.org",
        "maven.apache.org",
        // .NET & C# Frameworks
        "asp.net",
        "dotnet.microsoft.com",
        "nuget.org",
        "blazor.net",
        // Mobile Development
        "reactnative.dev",
        "docs.flutter.dev",
        "developer.apple.com",
        "developer.android.com",
        // Data Science & Machine Learning
        "keras.io",
        "spark.apache.org",
        "huggingface.co",
        "www.kaggle.com",
        // Databases
        "www.mongodb.com",
        "redis.io",
        "www.postgresql.org",
        "dev.mysql.com",
        "www.sqlite.org",
        "graphql.org",
        "prisma.io",
        // Cloud & DevOps
        "docs.aws.amazon.com",
        "cloud.google.com",
        "kubernetes.io",
        "www.docker.com",
        "www.terraform.io",
        "www.ansible.com",
        "docs.netlify.com",
        "devcenter.heroku.com",
        // Testing & Monitoring
        "cypress.io",
        "selenium.dev",
        // Game Development
        "docs.unity.com",
        "docs.unrealengine.com",
        // Other Essential Tools
        "git-scm.com",
        "nginx.org",
        "httpd.apache.org",
    ]
    .into_iter()
    .collect()
});

/// Path-scoped preapproved entries: host → list of allowed path prefixes. 1:1
/// with `PATH_PREFIXES` (`preapproved.ts:136-152`). Only the two TS entries with
/// a "/" land here: `github.com/anthropics` and `vercel.com/docs`.
static PATH_PREFIXES: Lazy<std::collections::HashMap<&'static str, &'static [&'static str]>> =
    Lazy::new(|| {
        let mut m = std::collections::HashMap::new();
        m.insert("github.com", &["/anthropics"][..]);
        m.insert("vercel.com", &["/docs"][..]);
        m
    });

/// `isPreapprovedHost(hostname, pathname)` — 1:1 with `preapproved.ts:154-166`.
/// Returns true when `host` is a hostname-only entry, or when `host` has a
/// path-prefix entry and `path` matches a prefix at a SEGMENT BOUNDARY (exact
/// match or the prefix followed by `/`) — so `/anthropics` matches but
/// `/anthropics-evil/malware` does not.
#[must_use]
pub fn is_preapproved_host(host: &str, path: &str) -> bool {
    if HOSTNAME_ONLY.contains(host) {
        return true;
    }
    if let Some(prefixes) = PATH_PREFIXES.get(host) {
        for p in *prefixes {
            // Enforce path-segment boundaries: "/anthropics" must not match
            // "/anthropics-evil/malware". Only an exact match, or a "/" right
            // after the prefix, is allowed (TS: `pathname === p ||
            // pathname.startsWith(p + '/')`).
            if path == *p || path.starts_with(&format!("{p}/")) {
                return true;
            }
        }
    }
    false
}

/// Whether `host` (ignoring path) is preapproved — used for the apply-step
/// guideline selection (`makeSecondaryModelPrompt`'s `isPreapprovedDomain`).
/// claude-code passes `isPreapprovedUrl(url)` (host+path) as that flag; this
/// host-only helper is the conservative analog used where only the host is in
/// hand. A host with ONLY a path-scoped entry (e.g. `github.com`, `vercel.com`)
/// returns false here — the full URL check in `is_preapproved_host` is the
/// authoritative gate for those.
#[must_use]
pub fn is_preapproved_domain(host: &str) -> bool {
    HOSTNAME_ONLY.contains(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_html_content_type() {
        assert!(is_html_content_type("text/html; charset=utf-8"));
        assert!(is_html_content_type("TEXT/HTML"));
        assert!(!is_html_content_type("text/markdown"));
        assert!(!is_html_content_type("application/json"));
        assert!(!is_html_content_type(""));
    }

    #[test]
    fn truncates_markdown_at_cap() {
        let big = "a".repeat(MAX_MARKDOWN_LENGTH + 10);
        let out = truncate_markdown(big);
        assert!(out.ends_with(crate::web_fetch::WEBFETCH_TRUNCATION_SUFFIX));
        let body = &out[..out.len() - crate::web_fetch::WEBFETCH_TRUNCATION_SUFFIX.len()];
        assert_eq!(body.len(), MAX_MARKDOWN_LENGTH);
    }

    #[test]
    fn does_not_truncate_short_markdown() {
        let s = "hello".to_string();
        assert_eq!(truncate_markdown(s.clone()), s);
    }

    #[test]
    fn truncates_by_utf16_units_bmp() {
        // `あ` = 1 UTF-16 unit, 3 bytes. JS slices to MAX UTF-16 units, so the
        // kept prefix is exactly `MAX` chars = `MAX*3` bytes (NOT `MAX` bytes).
        let big = "あ".repeat(MAX_MARKDOWN_LENGTH + 10);
        let out = truncate_markdown(big);
        assert!(out.ends_with(crate::web_fetch::WEBFETCH_TRUNCATION_SUFFIX));
        let body = &out[..out.len() - crate::web_fetch::WEBFETCH_TRUNCATION_SUFFIX.len()];
        assert_eq!(body.encode_utf16().count(), MAX_MARKDOWN_LENGTH);
        assert_eq!(body.chars().count(), MAX_MARKDOWN_LENGTH);
        assert_eq!(body.len(), MAX_MARKDOWN_LENGTH * 3);
    }

    #[test]
    fn truncates_by_utf16_units_astral() {
        // `😀` = 2 UTF-16 units, 1 scalar, 4 bytes. JS `.slice(0, MAX)` keeps
        // MAX/2 emojis (MAX UTF-16 units). The cut lands on a char boundary.
        let big = "😀".repeat(MAX_MARKDOWN_LENGTH); // 2*MAX units, well over cap
        let out = truncate_markdown(big);
        assert!(out.ends_with(crate::web_fetch::WEBFETCH_TRUNCATION_SUFFIX));
        let body = &out[..out.len() - crate::web_fetch::WEBFETCH_TRUNCATION_SUFFIX.len()];
        assert_eq!(body.encode_utf16().count(), MAX_MARKDOWN_LENGTH);
        assert_eq!(body.chars().count(), MAX_MARKDOWN_LENGTH / 2);
    }

    #[test]
    fn secondary_prompt_matches_template_strict() {
        let got = make_secondary_model_prompt("MD-HERE", "what is X?", false);
        let expected = "\nWeb page content:\n---\nMD-HERE\n---\n\nwhat is X?\n\n\
Provide a concise response based only on the content above. In your response:\n \
- Enforce a strict 125-character maximum for quotes from any source document. \
Open Source Software is ok as long as we respect the license.\n \
- Use quotation marks for exact language from articles; any language outside of \
the quotation should never be word-for-word the same.\n \
- You are not a lawyer and never comment on the legality of your own prompts and \
responses.\n - Never produce or reproduce exact song lyrics.\n";
        assert_eq!(got, expected);
    }

    #[test]
    fn secondary_prompt_preapproved_uses_short_guidelines() {
        let got = make_secondary_model_prompt("MD", "q", true);
        assert!(got.contains(
            "Provide a concise response based on the content above. Include relevant \
details, code examples, and documentation excerpts as needed."
        ));
        assert!(got.starts_with("\nWeb page content:\n---\nMD\n---\n\nq\n\n"));
    }

    // ---- preapproved hosts (preapproved.ts) --------------------------------

    #[test]
    fn hostname_only_hosts_are_preapproved() {
        // Path is irrelevant for hostname-only entries.
        assert!(is_preapproved_host(
            "doc.rust-lang.org",
            "/std/vec/index.html"
        ));
        assert!(is_preapproved_host("developer.mozilla.org", "/en-US/"));
        assert!(is_preapproved_host("react.dev", "/"));
        assert!(is_preapproved_host("learn.microsoft.com", "/anything")); // dedup'd dup entry
        assert!(is_preapproved_domain("doc.rust-lang.org"));
        assert!(is_preapproved_domain("developer.mozilla.org"));
    }

    #[test]
    fn unknown_host_is_not_preapproved() {
        assert!(!is_preapproved_host("evil.example.com", "/anthropics"));
        assert!(!is_preapproved_domain("evil.example.com"));
    }

    #[test]
    fn path_scoped_host_enforces_segment_boundaries() {
        // Exact prefix match and prefix-followed-by-"/" are allowed.
        assert!(is_preapproved_host("github.com", "/anthropics"));
        assert!(is_preapproved_host("github.com", "/anthropics/claude-code"));
        assert!(is_preapproved_host("vercel.com", "/docs"));
        assert!(is_preapproved_host("vercel.com", "/docs/functions"));
        // The exfil case from preapproved.ts:159-161 MUST be rejected.
        assert!(!is_preapproved_host(
            "github.com",
            "/anthropics-evil/malware"
        ));
        assert!(!is_preapproved_host("github.com", "/anthropicsevil"));
        assert!(!is_preapproved_host("github.com", "/someoneelse"));
        assert!(!is_preapproved_host("vercel.com", "/docsource"));
        // A path-scoped host is NOT a blanket preapproved DOMAIN (host-only check).
        assert!(!is_preapproved_domain("github.com"));
        assert!(!is_preapproved_domain("vercel.com"));
    }
}
