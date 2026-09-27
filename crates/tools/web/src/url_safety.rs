//! WebFetch URL-safety helpers — faithful port of the pure functions in
//! claude-code `src/tools/WebFetchTool/utils.ts` + `preapproved.ts`.
//!
//! - [`validate_url_safety`] — `validateURL`: reject overlong URLs, embedded
//!   credentials (`user:pass@`), and single-label / internal hostnames. Wired
//!   into [`crate::web_fetch::validate_url`] (a real fetch-time gate).
//! - [`is_preapproved_url`] / [`is_preapproved_host`] — `isPreapprovedUrl`: the
//!   ~90 code-doc hosts that bypass the domain blocklist. This module keeps the
//!   pure host/path predicate plus unit-test coverage, while the live WebFetch
//!   apply-step / markdown path currently consults
//!   [`crate::markdown::is_preapproved_host`] for the production gate.
//! - [`is_permitted_redirect`] — `isPermittedRedirect`: a redirect is safe only
//!   if it keeps protocol+port, carries no credentials, and stays on the same
//!   host modulo a leading `www.`. The live redirect gate is
//!   [`crate::web_fetch::is_permitted_redirect`]; this copy is the shared pure
//!   helper kept in lockstep with that fetch-time check.

use url::Url;

/// Maximum accepted URL length (claude-code `MAX_URL_LENGTH`).
pub const MAX_URL_LENGTH: usize = 2000;

/// `validateURL`: basic SSRF/abuse gate applied at fetch time. `raw` is the
/// original URL string (length is measured before parsing/normalization).
///
/// Faithful to utils.ts:139-169 — rejects: length > [`MAX_URL_LENGTH`]; a URL
/// carrying a username or password; a hostname with fewer than two dot-separated
/// labels (e.g. `localhost`, an intranet single-label host). Protocol is NOT
/// checked here (claude-code upgrades http→https later; the Rust caller keeps
/// its own scheme allow-list separately).
pub fn validate_url_safety(raw: &str, parsed: &Url) -> Result<(), String> {
    if raw.len() > MAX_URL_LENGTH {
        return Err(format!(
            "URL exceeds maximum length of {MAX_URL_LENGTH} characters"
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("URLs containing credentials (user:password@) are not allowed".into());
    }
    let host = parsed.host_str().unwrap_or("");
    // claude-code: `hostname.split('.')`, reject when `parts.length < 2`
    // (does NOT collapse empty labels — kept identical for parity).
    if host.split('.').count() < 2 {
        return Err("URL hostname is not publicly resolvable".into());
    }
    Ok(())
}

/// `isPermittedRedirect`: is following `redirect_url` from `original_url` safe?
/// Same protocol + port, no credentials on the redirect, and the same host
/// ignoring a single leading `www.` (add/remove www, or same host with a
/// different path/query). Faithful to utils.ts:212-243.
#[must_use]
pub fn is_permitted_redirect(original_url: &str, redirect_url: &str) -> bool {
    let (Ok(orig), Ok(redir)) = (Url::parse(original_url), Url::parse(redirect_url)) else {
        return false;
    };
    if redir.scheme() != orig.scheme() {
        return false;
    }
    // `port_or_known_default` so an explicit `:443` and an implicit https port
    // compare equal, matching how browsers treat the default port.
    if redir.port_or_known_default() != orig.port_or_known_default() {
        return false;
    }
    if !redir.username().is_empty() || redir.password().is_some() {
        return false;
    }
    strip_www(orig.host_str().unwrap_or("")) == strip_www(redir.host_str().unwrap_or(""))
}

fn strip_www(host: &str) -> &str {
    host.strip_prefix("www.").unwrap_or(host)
}

/// `isPreapprovedUrl`: does this URL's host (and path, for path-scoped entries)
/// fall in the preapproved code-doc allow-list? Unparseable → `false`.
#[must_use]
pub fn is_preapproved_url(url: &str) -> bool {
    match Url::parse(url) {
        Ok(u) => is_preapproved_host(u.host_str().unwrap_or(""), u.path()),
        Err(_) => false,
    }
}

/// `isPreapprovedHost`: hostname-only entries match the host outright;
/// path-scoped entries (e.g. `github.com/anthropics`) additionally require the
/// pathname to equal the prefix or start with `prefix + '/'` (segment boundary,
/// so `/anthropics` does not match `/anthropics-evil`). Faithful to
/// preapproved.ts:154-166.
#[must_use]
pub fn is_preapproved_host(hostname: &str, pathname: &str) -> bool {
    let (hosts, paths) = split_tables();
    if hosts.contains(&hostname) {
        return true;
    }
    if let Some(prefixes) = paths.get(hostname) {
        for p in prefixes {
            if pathname == *p || pathname.starts_with(&format!("{p}/")) {
                return true;
            }
        }
    }
    false
}

/// The preapproved entries, verbatim from preapproved.ts:14-131. Entries with a
/// `/` are path-scoped (`host/path`); the rest are hostname-only.
const PREAPPROVED_HOSTS: &[&str] = &[
    // Anthropic
    "platform.claude.com",
    "code.claude.com",
    "modelcontextprotocol.io",
    "github.com/anthropics",
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
    // (learn.microsoft.com appears twice in the TS Set — deduped here)
    "kubernetes.io",
    "www.docker.com",
    "www.terraform.io",
    "www.ansible.com",
    "vercel.com/docs",
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
];

type Tables = (
    std::collections::HashSet<&'static str>,
    std::collections::HashMap<&'static str, Vec<&'static str>>,
);

/// Split [`PREAPPROVED_HOSTS`] once into a hostname-only set + a per-host
/// path-prefix map (preapproved.ts:136-152), cached for O(1) lookups.
fn split_tables() -> &'static Tables {
    use std::sync::OnceLock;
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut hosts = std::collections::HashSet::new();
        let mut paths: std::collections::HashMap<&'static str, Vec<&'static str>> =
            std::collections::HashMap::new();
        for entry in PREAPPROVED_HOSTS {
            match entry.find('/') {
                None => {
                    hosts.insert(*entry);
                }
                Some(slash) => {
                    let host = &entry[..slash];
                    let path = &entry[slash..];
                    paths.entry(host).or_default().push(path);
                }
            }
        }
        (hosts, paths)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn rejects_credentials_in_url() {
        let u = url("https://user:pass@example.com/");
        assert!(validate_url_safety("https://user:pass@example.com/", &u).is_err());
    }

    #[test]
    fn rejects_single_label_host() {
        let u = url("https://localhost/");
        assert!(validate_url_safety("https://localhost/", &u).is_err());
        let u2 = url("http://intranet/");
        assert!(validate_url_safety("http://intranet/", &u2).is_err());
    }

    #[test]
    fn rejects_overlong_url() {
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LENGTH));
        let u = url("https://example.com/");
        assert!(validate_url_safety(&long, &u).is_err());
    }

    #[test]
    fn accepts_normal_public_url() {
        let u = url("https://docs.rs/serde/latest/serde/");
        assert!(validate_url_safety("https://docs.rs/serde/latest/serde/", &u).is_ok());
    }

    #[test]
    fn permitted_redirect_www_toggle_and_path() {
        assert!(is_permitted_redirect(
            "https://example.com/a",
            "https://www.example.com/b"
        ));
        assert!(is_permitted_redirect(
            "https://www.example.com/a",
            "https://example.com/"
        ));
        assert!(is_permitted_redirect(
            "https://example.com/a",
            "https://example.com/a?x=1"
        ));
    }

    #[test]
    fn forbidden_redirect_cross_host_scheme_creds() {
        assert!(!is_permitted_redirect(
            "https://example.com/",
            "https://evil.com/"
        ));
        assert!(!is_permitted_redirect(
            "https://example.com/",
            "http://example.com/"
        )); // protocol change
        assert!(!is_permitted_redirect(
            "https://example.com/",
            "https://user:pw@example.com/"
        )); // creds
    }

    #[test]
    fn preapproved_hostname_only() {
        assert!(is_preapproved_url(
            "https://docs.python.org/3/library/os.html"
        ));
        assert!(is_preapproved_url("https://react.dev/learn"));
        assert!(!is_preapproved_url("https://random-blog.example/"));
    }

    #[test]
    fn preapproved_path_scoped_segment_boundary() {
        // github.com/anthropics is path-scoped.
        assert!(is_preapproved_url("https://github.com/anthropics"));
        assert!(is_preapproved_url(
            "https://github.com/anthropics/claude-code"
        ));
        // segment boundary: /anthropics-evil must NOT match.
        assert!(!is_preapproved_url(
            "https://github.com/anthropics-evil/malware"
        ));
        // a non-anthropics github path is not preapproved.
        assert!(!is_preapproved_url("https://github.com/someone-else/repo"));
        // vercel.com/docs path scope.
        assert!(is_preapproved_url("https://vercel.com/docs/cli"));
        assert!(!is_preapproved_url("https://vercel.com/pricing"));
    }
}
