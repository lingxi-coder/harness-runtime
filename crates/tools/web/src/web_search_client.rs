//! Provider-agnostic CLIENT-SIDE web search.
//!
//! The hosted [`crate::web_search::WebSearchTool`] path only works on Anthropic's
//! first-party API (its `web_search_20250305` server tool). On other providers
//! (GitHub Copilot, OpenAI, …) `WebSearchTool` falls back to THIS module, which
//! runs the search itself over the injected [`lingxi_core::host::http::HttpTransport`] and
//! returns markdown result blocks the model can read — no provider hosting.
//!
//! Provider selection prefers credentials explicitly saved through LingXi's
//! settings/secure store, then environment fallbacks for headless use, with a
//! keyless DuckDuckGo fallback so it works zero-config:
//!   saved Tavily → `TAVILY_API_KEY` → saved Brave → `BRAVE_API_KEY` →
//!   `LINGXI_SEARXNG_URL` → DuckDuckGo Lite.

use crate::web_search_config::{WebSearchConfig, WebSearchProvider};
use lingxi_core::host::http::HttpTransport;
use lingxi_core::types::{HttpMethod, HttpRequest};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// One normalized search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Backend used for a client-side search.
#[derive(Debug, Clone)]
pub enum ClientSearchProvider {
    Tavily(String),
    Brave(String),
    Searxng(String),
    DuckDuckGo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebSearchKeyPresence {
    pub tavily: bool,
    pub brave: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWebCredentials {
    pub tavily_key: Option<String>,
    pub brave_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvSearchConfig {
    pub tavily_key: Option<String>,
    pub brave_key: Option<String>,
    pub searxng_url: Option<String>,
}

impl EnvSearchConfig {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            tavily_key: None,
            brave_key: None,
            searxng_url: None,
        }
    }

    #[must_use]
    pub fn from_env() -> Self {
        Self {
            tavily_key: env_trimmed("TAVILY_API_KEY"),
            brave_key: env_trimmed("BRAVE_API_KEY"),
            searxng_url: env_trimmed("LINGXI_SEARXNG_URL")
                .map(|u| u.trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty()),
        }
    }
}

impl ResolvedWebCredentials {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            tavily_key: None,
            brave_key: None,
        }
    }

    #[must_use]
    pub fn from_env() -> Self {
        let env = EnvSearchConfig::from_env();
        Self {
            tavily_key: env.tavily_key,
            brave_key: env.brave_key,
        }
    }
}

const CLIENT_SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_MAX_RESULTS: usize = 8;

impl ClientSearchProvider {
    /// Resolve the backend from env keys, preferring higher-quality providers;
    /// always resolves (DuckDuckGo is keyless), so client search works zero-config.
    ///
    /// This helper has no access to LingXi's secure store, so it treats env
    /// values as the only configured credentials.
    #[must_use]
    pub fn from_env() -> Self {
        let cfg = WebSearchConfig::default();
        let secure = ResolvedWebCredentials::from_env();
        let env = EnvSearchConfig::from_env();
        resolve_client_search_provider_with_credentials(&cfg, &secure, &env)
            .unwrap_or(Self::DuckDuckGo)
    }

    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Tavily(_) => "Tavily",
            Self::Brave(_) => "Brave Search",
            Self::Searxng(_) => "SearXNG",
            Self::DuckDuckGo => "DuckDuckGo",
        }
    }
}

#[must_use]
pub fn resolve_client_search_provider(
    cfg: &WebSearchConfig,
    keys: &WebSearchKeyPresence,
    env: EnvSearchConfig,
) -> Result<ClientSearchProvider, String> {
    let secure = ResolvedWebCredentials {
        tavily_key: keys.tavily.then(|| "secure-tavily".to_string()),
        brave_key: keys.brave.then(|| "secure-brave".to_string()),
    };
    resolve_client_search_provider_with_credentials(cfg, &secure, &env)
}

#[must_use]
pub fn resolve_client_search_provider_with_credentials(
    cfg: &WebSearchConfig,
    secure: &ResolvedWebCredentials,
    env: &EnvSearchConfig,
) -> Result<ClientSearchProvider, String> {
    let candidates = resolve_client_search_candidates(cfg, secure, env);
    candidates
        .into_iter()
        .next()
        .ok_or_else(|| match cfg.provider {
            WebSearchProvider::Tavily => {
                "Tavily is selected but no API key is configured".to_string()
            }
            WebSearchProvider::Brave => {
                "Brave is selected but no API key is configured".to_string()
            }
            WebSearchProvider::Searxng => {
                "SearXNG is selected but no URL is configured".to_string()
            }
            WebSearchProvider::DuckDuckGo | WebSearchProvider::Auto => {
                "No web search provider is configured".to_string()
            }
        })
}

#[must_use]
/// Return the ordered client-side search candidates for the configured backend.
///
/// `Auto` keeps every configured provider in fallback order so a stale key
/// cannot block the next credential source or the keyless fallback. Secure-store
/// credentials are tried before environment variables because `/web` writes are
/// the explicit in-app configuration, while env vars are primarily a
/// headless/CI fallback.
pub fn resolve_client_search_candidates(
    cfg: &WebSearchConfig,
    secure: &ResolvedWebCredentials,
    env: &EnvSearchConfig,
) -> Vec<ClientSearchProvider> {
    let searxng_url = cfg.searxng_url.clone().or_else(|| {
        env.searxng_url
            .clone()
            .map(|u| u.trim_end_matches('/').to_string())
    });

    fn push_tavily(out: &mut Vec<ClientSearchProvider>, key: Option<String>) {
        if let Some(key) = key {
            if !out
                .iter()
                .any(|p| matches!(p, ClientSearchProvider::Tavily(existing) if existing == &key))
            {
                out.push(ClientSearchProvider::Tavily(key));
            }
        }
    }
    fn push_brave(out: &mut Vec<ClientSearchProvider>, key: Option<String>) {
        if let Some(key) = key {
            if !out
                .iter()
                .any(|p| matches!(p, ClientSearchProvider::Brave(existing) if existing == &key))
            {
                out.push(ClientSearchProvider::Brave(key));
            }
        }
    }
    fn push_searxng(out: &mut Vec<ClientSearchProvider>, url: Option<String>) {
        if let Some(url) = url {
            if !out
                .iter()
                .any(|p| matches!(p, ClientSearchProvider::Searxng(existing) if existing == &url))
            {
                out.push(ClientSearchProvider::Searxng(url));
            }
        }
    }

    let mut out = Vec::new();

    match cfg.provider {
        WebSearchProvider::Tavily => {
            push_tavily(&mut out, secure.tavily_key.clone());
            push_tavily(&mut out, env.tavily_key.clone());
        }
        WebSearchProvider::Brave => {
            push_brave(&mut out, secure.brave_key.clone());
            push_brave(&mut out, env.brave_key.clone());
        }
        WebSearchProvider::Searxng => push_searxng(&mut out, searxng_url),
        WebSearchProvider::DuckDuckGo => out.push(ClientSearchProvider::DuckDuckGo),
        WebSearchProvider::Auto => {
            push_tavily(&mut out, secure.tavily_key.clone());
            push_tavily(&mut out, env.tavily_key.clone());
            push_brave(&mut out, secure.brave_key.clone());
            push_brave(&mut out, env.brave_key.clone());
            push_searxng(&mut out, searxng_url);
            out.push(ClientSearchProvider::DuckDuckGo);
        }
    }
    out
}

/// Run a client-side web search and return normalized hits (already
/// domain-filtered and capped). `Err` carries a model-facing message.
///
/// # Errors
/// Returns a message string on transport failure or an empty result set.
pub async fn run_client_web_search(
    http: &Arc<dyn HttpTransport>,
    provider: &ClientSearchProvider,
    query: &str,
    allowed_domains: &[String],
    blocked_domains: &[String],
    max_results: usize,
) -> Result<Vec<SearchHit>, String> {
    let max_results = if max_results == 0 {
        DEFAULT_MAX_RESULTS
    } else {
        max_results
    };
    let (req, parse): (HttpRequest, fn(&str) -> Vec<SearchHit>) = match provider {
        ClientSearchProvider::Tavily(key) => {
            (tavily_request(key, query, max_results), parse_tavily_body)
        }
        ClientSearchProvider::Brave(key) => {
            (brave_request(key, query, max_results), parse_brave_body)
        }
        ClientSearchProvider::Searxng(base) => (searxng_request(base, query), parse_searxng_body),
        ClientSearchProvider::DuckDuckGo => (ddg_lite_request(query), parse_ddg_lite_html),
    };
    let resp = http
        .request(req)
        .await
        .map_err(|e| format!("web search transport error ({}): {e}", provider.label()))?;
    if resp.status >= 400 {
        // Surface the provider's response body (e.g. Tavily returns
        // `{"detail":{"error":"Unauthorized: missing or invalid API key"}}`)
        // so an auth failure is distinguishable from a wrong-key failure.
        let detail = resp.body.trim();
        let detail = detail.chars().take(200).collect::<String>();
        return Err(format!(
            "web search failed ({}): HTTP {}{}",
            provider.label(),
            resp.status,
            if detail.is_empty() {
                String::new()
            } else {
                format!(" — {detail}")
            }
        ));
    }
    let mut hits = parse(&resp.body);
    hits = apply_domain_filter(hits, allowed_domains, blocked_domains);
    hits.truncate(max_results);
    if hits.is_empty() {
        return Err(format!(
            "No results from {} for \"{query}\".",
            provider.label()
        ));
    }
    Ok(hits)
}

fn enc(q: &str) -> String {
    url::form_urlencoded::byte_serialize(q.as_bytes()).collect()
}

fn env_trimmed(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ── request builders ────────────────────────────────────────────────────────

fn tavily_request(key: &str, query: &str, max_results: usize) -> HttpRequest {
    // Tavily authenticates via an `Authorization: Bearer <key>` header (its
    // OpenAPI `bearerAuth` security scheme). The legacy body `api_key` field is
    // no longer accepted and returns HTTP 401.
    let body = json!({
        "query": query,
        "max_results": max_results,
        "search_depth": "basic",
    });
    HttpRequest {
        method: HttpMethod::Post,
        url: "https://api.tavily.com/search".to_string(),
        headers: vec![
            ("content-type".into(), "application/json".into()),
            ("authorization".into(), format!("Bearer {key}")),
        ],
        body: Some(body.to_string()),
        body_bytes: None,
        timeout: Some(CLIENT_SEARCH_TIMEOUT),
    }
}

fn brave_request(key: &str, query: &str, max_results: usize) -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: format!(
            "https://api.search.brave.com/res/v1/web/search?q={}&count={max_results}",
            enc(query)
        ),
        headers: vec![
            ("accept".into(), "application/json".into()),
            ("x-subscription-token".into(), key.to_string()),
        ],
        body: None,
        body_bytes: None,
        timeout: Some(CLIENT_SEARCH_TIMEOUT),
    }
}

fn searxng_request(base: &str, query: &str) -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: format!("{base}/search?q={}&format=json", enc(query)),
        headers: vec![("accept".into(), "application/json".into())],
        body: None,
        body_bytes: None,
        timeout: Some(CLIENT_SEARCH_TIMEOUT),
    }
}

fn ddg_lite_request(query: &str) -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: format!("https://lite.duckduckgo.com/lite/?q={}", enc(query)),
        headers: vec![
            (
                "user-agent".into(),
                "Mozilla/5.0 (compatible; LingXi-Code)".into(),
            ),
            ("accept".into(), "text/html".into()),
        ],
        body: None,
        body_bytes: None,
        timeout: Some(CLIENT_SEARCH_TIMEOUT),
    }
}

// ── response parsers (pure, unit-tested) ─────────────────────────────────────

fn parse_tavily_body(body: &str) -> Vec<SearchHit> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return vec![];
    };
    v.get("results")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let url = r.get("url").and_then(Value::as_str)?.to_string();
                    Some(SearchHit {
                        title: r
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        url,
                        snippet: r
                            .get("content")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_brave_body(body: &str) -> Vec<SearchHit> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return vec![];
    };
    v.get("web")
        .and_then(|w| w.get("results"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let url = r.get("url").and_then(Value::as_str)?.to_string();
                    Some(SearchHit {
                        title: r
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        url,
                        snippet: r
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_searxng_body(body: &str) -> Vec<SearchHit> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return vec![];
    };
    v.get("results")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let url = r.get("url").and_then(Value::as_str)?.to_string();
                    Some(SearchHit {
                        title: r
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        url,
                        snippet: r
                            .get("content")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse DuckDuckGo Lite HTML. Lite renders each result as an
/// `<a ... class="result-link" href="…">Title</a>` followed by a
/// `<td class="result-snippet">snippet</td>`. The href is a
/// `//duckduckgo.com/l/?uddg=<percent-encoded-real-url>` redirect wrapper.
/// Best-effort string scanning (no HTML crate); resilient to missing snippets.
fn parse_ddg_lite_html(html: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = html[search_from..].find("<a ") {
        let tag_start = search_from + rel;
        let Some(gt_rel) = html[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + gt_rel;
        let open_tag = &html[tag_start..tag_end]; // `<a ... `
        search_from = tag_end + 1;
        if !open_tag.contains("result-link") {
            continue;
        }
        let Some(href) = attr_value(open_tag, "href") else {
            continue;
        };
        let url = unwrap_ddg_redirect(&href);
        if url.is_empty() {
            continue;
        }
        // Title = inner text up to </a>.
        let title = html[tag_end + 1..]
            .find("</a>")
            .map(|e| strip_tags(&html[tag_end + 1..tag_end + 1 + e]))
            .unwrap_or_default();
        // Snippet = the next result-snippet cell before the next result-link.
        let after = &html[search_from..];
        let next_link = after.find("result-link").unwrap_or(after.len());
        let snippet = after[..next_link]
            .find("result-snippet")
            .and_then(|s| {
                let seg = &after[s..next_link];
                seg.find('>').and_then(|gt| {
                    seg[gt + 1..]
                        .find("</td>")
                        .map(|e| strip_tags(&seg[gt + 1..gt + 1 + e]))
                })
            })
            .unwrap_or_default();
        hits.push(SearchHit {
            title: decode_entities(&title),
            url,
            snippet: decode_entities(&snippet),
        });
    }
    hits
}

/// Extract an HTML attribute value (single or double quoted) from the first tag
/// in `s` that contains `name=`.
fn attr_value(s: &str, name: &str) -> Option<String> {
    let key = format!("{name}=");
    let i = s.find(&key)? + key.len();
    let bytes = s.as_bytes();
    let quote = *bytes.get(i)?;
    if quote == b'"' || quote == b'\'' {
        let q = quote as char;
        let end = s[i + 1..].find(q)? + i + 1;
        Some(s[i + 1..end].to_string())
    } else {
        let end = s[i..].find([' ', '>'])? + i;
        Some(s[i..end].to_string())
    }
}

/// Resolve a `//duckduckgo.com/l/?uddg=<enc>` redirect wrapper to the real URL.
fn unwrap_ddg_redirect(href: &str) -> String {
    let normalized = if let Some(stripped) = href.strip_prefix("//") {
        format!("https://{stripped}")
    } else {
        href.to_string()
    };
    if let Ok(u) = url::Url::parse(&normalized) {
        if u.path() == "/l/" {
            if let Some((_, val)) = u.query_pairs().find(|(k, _)| k == "uddg") {
                return val.into_owned();
            }
        }
    }
    normalized
}

/// Strip HTML tags from a fragment (DDG bolds query terms with `<b>`).
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Decode the handful of HTML entities DDG/Brave emit in titles/snippets.
fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

// ── filtering + formatting ───────────────────────────────────────────────────

fn host_of(u: &str) -> Option<String> {
    url::Url::parse(u).ok().and_then(|p| {
        p.host_str()
            .map(|h| h.trim_start_matches("www.").to_string())
    })
}

fn domain_matches(host: &str, domain: &str) -> bool {
    let host = host.trim_start_matches("www.").to_ascii_lowercase();
    let domain = domain.trim_start_matches("www.").to_ascii_lowercase();
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Apply allow/block domain filters (substring match on the host, matching the
/// hosted tool's `allowed_domains`/`blocked_domains` semantics).
#[must_use]
pub fn apply_domain_filter(
    hits: Vec<SearchHit>,
    allowed: &[String],
    blocked: &[String],
) -> Vec<SearchHit> {
    hits.into_iter()
        .filter(|h| {
            let host = host_of(&h.url).unwrap_or_default();
            let allow_ok = allowed.is_empty() || allowed.iter().any(|d| domain_matches(&host, d));
            let block_ok = blocked.iter().all(|d| !domain_matches(&host, d));
            allow_ok && block_ok
        })
        .collect()
}

/// Format hits into a model-facing markdown block, mirroring the hosted tool's
/// "links as markdown hyperlinks + Sources" convention so the model cites them.
#[must_use]
pub fn format_results_for_model(query: &str, hits: &[SearchHit], provider_label: &str) -> String {
    let mut out = format!("Web search results for \"{query}\" (via {provider_label}):\n\n");
    for (i, h) in hits.iter().enumerate() {
        let title = if h.title.is_empty() { &h.url } else { &h.title };
        out.push_str(&format!("{}. [{}]({})\n", i + 1, title, h.url));
        if !h.snippet.is_empty() {
            out.push_str(&format!("   {}\n", h.snippet));
        }
    }
    out.push_str("\nSources:\n");
    for h in hits {
        let title = if h.title.is_empty() { &h.url } else { &h.title };
        out.push_str(&format!("- [{}]({})\n", title, h.url));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_search_config::{WebSearchConfig, WebSearchProvider};

    mod resolve_client_search_provider {
        use super::*;

        #[test]
        fn active_specific_provider_requires_configured_key() {
            let cfg = WebSearchConfig {
                provider: WebSearchProvider::Tavily,
                searxng_url: None,
            };
            let keys = WebSearchKeyPresence {
                tavily: false,
                brave: false,
            };
            let err =
                resolve_client_search_provider(&cfg, &keys, EnvSearchConfig::empty()).unwrap_err();
            assert!(err.contains("Tavily is selected but no API key is configured"));
        }

        #[test]
        fn active_specific_provider_uses_env_key_when_secure_key_absent() {
            let cfg = WebSearchConfig {
                provider: WebSearchProvider::Tavily,
                searxng_url: None,
            };
            let keys = WebSearchKeyPresence {
                tavily: false,
                brave: false,
            };
            let env = EnvSearchConfig {
                tavily_key: Some("env-tavily".into()),
                brave_key: None,
                searxng_url: None,
            };
            assert!(
                matches!(resolve_client_search_provider(&cfg, &keys, env).unwrap(), ClientSearchProvider::Tavily(k) if k == "env-tavily")
            );
        }

        #[test]
        fn active_specific_provider_prefers_secure_key_over_env_key() {
            let cfg = WebSearchConfig {
                provider: WebSearchProvider::Tavily,
                searxng_url: None,
            };
            let secure = ResolvedWebCredentials {
                tavily_key: Some("secure-tavily".into()),
                brave_key: None,
            };
            let env = EnvSearchConfig {
                tavily_key: Some("env-tavily".into()),
                brave_key: None,
                searxng_url: None,
            };
            assert!(
                matches!(resolve_client_search_provider_with_credentials(&cfg, &secure, &env).unwrap(), ClientSearchProvider::Tavily(k) if k == "secure-tavily")
            );
        }

        #[test]
        fn auto_candidates_try_secure_before_env_and_keep_fallbacks() {
            let cfg = WebSearchConfig::default();
            let secure = ResolvedWebCredentials {
                tavily_key: Some("secure-tavily".into()),
                brave_key: None,
            };
            let env = EnvSearchConfig {
                tavily_key: Some("env-tavily".into()),
                brave_key: None,
                searxng_url: None,
            };
            let candidates = resolve_client_search_candidates(&cfg, &secure, &env);
            assert_eq!(candidates.len(), 3);
            assert!(
                matches!(&candidates[0], ClientSearchProvider::Tavily(k) if k == "secure-tavily")
            );
            assert!(matches!(&candidates[1], ClientSearchProvider::Tavily(k) if k == "env-tavily"));
            assert!(matches!(candidates[2], ClientSearchProvider::DuckDuckGo));
        }

        #[test]
        fn auto_fallback_prefers_secure_keys_then_searxng_then_duckduckgo() {
            let cfg = WebSearchConfig::default();
            assert!(matches!(
                resolve_client_search_provider(
                    &cfg,
                    &WebSearchKeyPresence {
                        tavily: true,
                        brave: true
                    },
                    EnvSearchConfig::empty()
                )
                .unwrap(),
                ClientSearchProvider::Tavily(_)
            ));
            assert!(matches!(
                resolve_client_search_provider(
                    &cfg,
                    &WebSearchKeyPresence {
                        tavily: false,
                        brave: true
                    },
                    EnvSearchConfig::empty()
                )
                .unwrap(),
                ClientSearchProvider::Brave(_)
            ));
            assert!(matches!(
                resolve_client_search_provider(
                    &WebSearchConfig {
                        provider: WebSearchProvider::Auto,
                        searxng_url: Some("https://s.example".into())
                    },
                    &WebSearchKeyPresence {
                        tavily: false,
                        brave: false
                    },
                    EnvSearchConfig::empty()
                )
                .unwrap(),
                ClientSearchProvider::Searxng(_)
            ));
            assert!(matches!(
                resolve_client_search_provider(
                    &cfg,
                    &WebSearchKeyPresence {
                        tavily: false,
                        brave: false
                    },
                    EnvSearchConfig::empty()
                )
                .unwrap(),
                ClientSearchProvider::DuckDuckGo
            ));
        }
    }

    #[test]
    fn tavily_parser_extracts_hits() {
        let body = r#"{"results":[{"title":"T1","url":"https://a.com","content":"snip1"},{"title":"T2","url":"https://b.com","content":"snip2"}]}"#;
        let hits = parse_tavily_body(body);
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0],
            SearchHit {
                title: "T1".into(),
                url: "https://a.com".into(),
                snippet: "snip1".into()
            }
        );
    }

    #[test]
    fn brave_parser_extracts_nested_results() {
        let body = r#"{"web":{"results":[{"title":"B","url":"https://x.com","description":"d"}]}}"#;
        let hits = parse_brave_body(body);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://x.com");
        assert_eq!(hits[0].snippet, "d");
    }

    #[test]
    fn searxng_parser_extracts_results() {
        let body = r#"{"results":[{"title":"S","url":"https://s.com","content":"c"}]}"#;
        let hits = parse_searxng_body(body);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "S");
    }

    #[test]
    fn ddg_lite_parser_unwraps_redirect_and_strips_tags() {
        let html = r#"<a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=x" class="result-link">Example <b>Title</b></a><td class="result-snippet">A <b>snippet</b> here</td>"#;
        let hits = parse_ddg_lite_html(html);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.com/page");
        assert_eq!(hits[0].title, "Example Title");
        assert_eq!(hits[0].snippet, "A snippet here");
    }

    #[test]
    fn domain_filter_allow_and_block() {
        let hits = vec![
            SearchHit {
                title: "a".into(),
                url: "https://good.com/x".into(),
                snippet: String::new(),
            },
            SearchHit {
                title: "b".into(),
                url: "https://bad.com/y".into(),
                snippet: String::new(),
            },
        ];
        let allowed = apply_domain_filter(hits.clone(), &["good.com".into()], &[]);
        assert_eq!(allowed.len(), 1);
        assert_eq!(allowed[0].url, "https://good.com/x");
        let blocked = apply_domain_filter(hits, &[], &["bad.com".into()]);
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].url, "https://good.com/x");
    }

    #[test]
    fn domain_filter_matches_domain_boundaries_not_substrings() {
        let hits = vec![
            SearchHit {
                title: "real".into(),
                url: "https://weather.com/today".into(),
                snippet: String::new(),
            },
            SearchHit {
                title: "fake".into(),
                url: "https://fakeweather.com/today".into(),
                snippet: String::new(),
            },
            SearchHit {
                title: "sub".into(),
                url: "https://news.weather.com/today".into(),
                snippet: String::new(),
            },
        ];

        let allowed = apply_domain_filter(hits.clone(), &["weather.com".into()], &[]);
        assert_eq!(
            allowed.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(),
            vec!["real", "sub"]
        );

        let blocked = apply_domain_filter(hits, &[], &["weather.com".into()]);
        assert_eq!(
            blocked.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(),
            vec!["fake"]
        );
    }

    #[test]
    fn format_includes_links_and_sources() {
        let hits = vec![SearchHit {
            title: "T".into(),
            url: "https://u.com".into(),
            snippet: "s".into(),
        }];
        let out = format_results_for_model("q", &hits, "Tavily");
        assert!(out.contains("[T](https://u.com)"));
        assert!(out.contains("Sources:"));
        assert!(out.contains("via Tavily"));
    }

    #[test]
    fn entity_decode() {
        assert_eq!(decode_entities("a &amp; b &#x27;c&#x27;"), "a & b 'c'");
    }
}
