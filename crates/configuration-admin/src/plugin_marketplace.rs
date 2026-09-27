//! `plugin marketplace list` — render the configured-marketplaces registry.
//!
//! claude-code's `plugin marketplace list` reads ONLY the resolved registry
//! `<plugins>/known_marketplaces.json` (probed: a settings-only
//! `extraKnownMarketplaces` declaration WITHOUT a registry entry renders
//! nothing; a registry entry WITHOUT a settings declaration still renders). The
//! registry is a map `name → { source, installLocation, lastUpdated }` where
//! `source` is one of:
//! `{source:"directory",path}` / `{source:"git",url,ref?}` /
//! `{source:"github",repo,ref?}` / `{source:"url",url}`.
//!
//! Output is 1:1 with the binary (verified live for the `directory` case; the
//! git/github/url human render mirrors the binary's exact template
//! `Source: Git (${url}${ref?`@${ref}`:""})` etc.).
//!
//! `add` / `remove` / `update` WRITE the registry + the per-scope
//! `extraKnownMarketplaces` settings declaration. `add` classifies its source
//! 1:1 with the binary's `Idr` resolver — a local **directory** (verified 1:1),
//! a **github** `owner/repo` shorthand or a **git** clone URL (both cloned via
//! `MarketplaceManager::resolve_index_via_git` into
//! `<plugins>/marketplaces/<name>/`), or a hosted **url** `marketplace.json`.
//!
//! A `ref` on a github/git source is checked out by the shared clone helper;
//! the network add path emits a single result
//! line rather than the binary's live per-step progress
//! ("Refreshing marketplace cache…", "Cloning repository…", …).

use std::path::{Path, PathBuf};

use migrations::settings_update::{read_settings_map, update_settings};
use serde_json::{Map, Value};

use crate::plugin_policy;
use crate::plugin_policy::MarketplaceSourceIdentity;
use crate::plugin_settings::{parse_scope_str, scope_label, scope_path, SCOPES};
use protocol::WritableScope;

/// The resolved-marketplaces registry file under the plugins root.
fn registry_path(plugins_dir: &Path) -> PathBuf {
    plugins_dir.join("known_marketplaces.json")
}

/// Load the registry `name → entry` map (missing / malformed / non-object ⇒
/// empty — resilient, matching the read-only boot).
fn load_registry(plugins_dir: &Path) -> Map<String, Value> {
    std::fs::read_to_string(registry_path(plugins_dir))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// The `source` sub-object of a registry entry (`{}` when absent).
fn source_of(entry: &Value) -> Map<String, Value> {
    entry
        .get("source")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// A source string field (`path` / `url` / `repo` / `ref`).
fn str_field(source: &Map<String, Value>, key: &str) -> String {
    source
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The `Source: …` human line for one entry, mirroring the binary's template.
fn source_render(entry: &Value) -> String {
    let source = source_of(entry);
    let kind = str_field(&source, "source");
    let ref_suffix = {
        let r = str_field(&source, "ref");
        if r.is_empty() {
            String::new()
        } else {
            format!("@{r}")
        }
    };
    match kind.as_str() {
        "directory" => format!("Directory ({})", str_field(&source, "path")),
        "git" => format!("Git ({}{ref_suffix})", str_field(&source, "url")),
        "github" => format!("GitHub ({}{ref_suffix})", str_field(&source, "repo")),
        "url" => format!("URL ({})", str_field(&source, "url")),
        // Unknown/absent source kind: render the raw kind for visibility rather
        // than fabricate a label.
        other => format!("{other} ()"),
    }
}

/// The `--json` object for one entry: `{name, source, <path|url|repo>, installLocation}`
/// (field set verified live for `directory`; git/github/url mirror the same
/// shape with their source-specific locator).
fn json_entry(name: &str, entry: &Value) -> Value {
    let source = source_of(entry);
    let kind = str_field(&source, "source");
    let mut out = Map::new();
    out.insert("name".to_string(), Value::String(name.to_string()));
    out.insert("source".to_string(), Value::String(kind.clone()));
    match kind.as_str() {
        "directory" => {
            out.insert(
                "path".to_string(),
                Value::String(str_field(&source, "path")),
            );
        }
        "github" => {
            out.insert(
                "repo".to_string(),
                Value::String(str_field(&source, "repo")),
            );
        }
        // git + url both locate via `url`.
        _ => {
            out.insert("url".to_string(), Value::String(str_field(&source, "url")));
        }
    }
    if let Some(loc) = entry.get("installLocation").and_then(Value::as_str) {
        out.insert(
            "installLocation".to_string(),
            Value::String(loc.to_string()),
        );
    }
    Value::Object(out)
}

/// `plugin marketplace list [--json]` — returns the text to print to stdout.
pub fn run_list(plugins_dir: &Path, json: bool) -> String {
    let registry = load_registry(plugins_dir);

    if json {
        let arr: Vec<Value> = registry
            .iter()
            .map(|(name, entry)| json_entry(name, entry))
            .collect();
        return serde_json::to_string_pretty(&Value::Array(arr))
            .unwrap_or_else(|_| "[]".to_string());
    }

    if registry.is_empty() {
        return "No marketplaces configured".to_string();
    }

    let mut lines = vec!["Configured marketplaces:".to_string(), String::new()];
    for (name, entry) in &registry {
        lines.push(format!("  ❯ {name}"));
        lines.push(format!("    Source: {}", source_render(entry)));
    }
    lines.join("\n")
}

/// Current UTC time as ISO-8601 with millisecond precision + `Z`
/// (`2026-07-04T12:04:33.514Z`), matching the registry's `lastUpdated`.
fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Write the registry map back (pretty, NO trailing newline — matching the
/// binary's `known_marketplaces.json`).
fn write_registry(plugins_dir: &Path, map: &Map<String, Value>) -> Result<(), String> {
    let serialized = serde_json::to_string_pretty(&Value::Object(map.clone()))
        .map_err(|e| format!("Failed to serialize registry: {e}"))?;
    std::fs::create_dir_all(plugins_dir)
        .map_err(|e| format!("Failed to create {}: {e}", plugins_dir.display()))?;
    platform_api::rooted_fs::atomic_write(
        plugins_dir,
        Path::new("known_marketplaces.json"),
        serialized.as_bytes(),
        platform_api::AtomicWriteOptions::default(),
    )
    .map_err(|e| {
        format!(
            "Failed to write {}: {e}",
            registry_path(plugins_dir).display()
        )
    })
}

/// The `extraKnownMarketplaces` declaration map from a scope's settings file.
fn read_extra(scope: WritableScope, home: &Path, cwd: &Path) -> Map<String, Value> {
    read_settings_map(&scope_path(scope, home, cwd))
        .ok()
        .and_then(|m| {
            let canonical = m.get("extraKnownMarketplaces").and_then(Value::as_object);
            let alias = m.get("additionalMarketplaces").and_then(Value::as_object);
            if canonical.is_some() && alias.is_some() {
                tracing::warn!(
                    "additionalMarketplaces is ignored because extraKnownMarketplaces is also set"
                );
            }
            canonical.or(alias).cloned()
        })
        .unwrap_or_default()
}

/// Read-modify-write a scope's `extraKnownMarketplaces` map.
fn write_extra(
    scope: WritableScope,
    home: &Path,
    cwd: &Path,
    map: Map<String, Value>,
) -> Result<(), String> {
    update_settings(
        &scope_path(scope, home, cwd),
        vec![(
            "extraKnownMarketplaces".to_string(),
            Some(Value::Object(map)),
        )],
    )
}

/// The marketplace-family invalid-scope error (distinct wording from the
/// enable/disable and install families — verified against the binary).
fn market_invalid_scope(s: &str) -> String {
    format!("✘ Invalid scope '{s}'. Use: user, project, or local")
}

/// Does any editable scope declare `name` in `extraKnownMarketplaces`?
fn declaring_scopes(name: &str, home: &Path, cwd: &Path) -> Vec<WritableScope> {
    SCOPES
        .into_iter()
        .filter(|s| read_extra(*s, home, cwd).contains_key(name))
        .collect()
}

/// A classified `plugin marketplace add <source>` argument.
///
/// Mirrors the binary's `Idr(source)` resolver: an SSH / HTTP(S) / local /
/// GitHub-shorthand string is normalized to one of these variants, each of which
/// maps to a settings + registry `source` object of a distinct shape
/// (`{source:"directory",path}` / `{source:"github",repo,ref?}` /
/// `{source:"git",url,ref?}` / `{source:"url",url}`).
#[derive(Debug, Clone, PartialEq)]
enum Source {
    /// A local directory holding a `.lingxi-plugin/marketplace.json`.
    Directory(PathBuf),
    /// A GitHub `owner/repo` shorthand (cloned via `github.com`).
    Github {
        repo: String,
        git_ref: Option<String>,
    },
    /// A full git clone URL (SSH `git@…`, or an HTTP(S) URL ending `.git` /
    /// containing `/_git/`, or a `github.com/owner/repo` HTTP URL — `.git`
    /// appended).
    Git {
        url: String,
        git_ref: Option<String>,
    },
    /// A hosted `marketplace.json` fetched over HTTP(S) (no git clone).
    Url { url: String },
}

/// The `source` tag string for one classified source (`m.source` in the
/// oracle) — used both for the `--sparse` guard's error message and for
/// `source_object`'s `"source"` field.
fn source_kind(source: &Source) -> &'static str {
    match source {
        Source::Directory(_) => "directory",
        Source::Github { .. } => "github",
        Source::Git { .. } => "git",
        Source::Url { .. } => "url",
    }
}

fn source_identity(source: &Source) -> MarketplaceSourceIdentity {
    match source {
        Source::Directory(path) => MarketplaceSourceIdentity::Directory {
            path: path.display().to_string(),
        },
        Source::Github { repo, git_ref } => MarketplaceSourceIdentity::Github {
            repo: repo.clone(),
            git_ref: git_ref.clone(),
            path: None,
        },
        Source::Git { url, git_ref } => MarketplaceSourceIdentity::Git {
            url: url.clone(),
            git_ref: git_ref.clone(),
            path: None,
        },
        Source::Url { url } => MarketplaceSourceIdentity::Url { url: url.clone() },
    }
}

fn lexical_absolute(path: &Path, cwd: &Path) -> PathBuf {
    let expanded = path
        .to_str()
        .and_then(|raw| raw.strip_prefix("~/"))
        .and_then(|rest| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest)))
        .unwrap_or_else(|| path.to_path_buf());
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let mut normalized = PathBuf::new();
    for component in candidate.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Resolve only lexical source identity. This deliberately performs no
/// filesystem or network access so managed policy can reject before side
/// effects.
fn preflight_source_identity(
    source: &str,
    cwd: &Path,
) -> Result<MarketplaceSourceIdentity, String> {
    let trimmed = source.trim();
    if trimmed.starts_with("./")
        || trimmed.starts_with("../")
        || trimmed.starts_with('/')
        || trimmed.starts_with('~')
    {
        return Ok(MarketplaceSourceIdentity::Directory {
            path: lexical_absolute(Path::new(trimmed), cwd)
                .display()
                .to_string(),
        });
    }
    classify_source(source).map(|source| source_identity(&source))
}

/// The unclassifiable-source error (binary: `cli_marketplace_add_invalid_source`).
fn invalid_source_format() -> String {
    "✘ Invalid marketplace source format. Try: owner/repo, https://..., or ./path".to_string()
}

/// The has-`/` but malformed GitHub shorthand error (binary-verbatim).
fn invalid_shorthand(t: &str) -> String {
    format!(
        "✘ '{t}' is not a valid GitHub owner/repo shorthand. For a git repo, use the full https:// clone URL from your host (typically ending in .git — some hosts like Azure DevOps omit it). For a hosted marketplace.json, use its https:// URL. For a local path, use ./ or an absolute path."
    )
}

/// GitHub owner segment: `[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?`.
fn valid_owner(o: &str) -> bool {
    let b = o.as_bytes();
    if b.is_empty() {
        return false;
    }
    let alnum = |c: u8| c.is_ascii_alphanumeric();
    if !alnum(b[0]) || !alnum(b[b.len() - 1]) {
        return false;
    }
    o.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// GitHub `owner/repo` shorthand: owner as above, `/`, then a repo of
/// `[A-Za-z0-9._-]+` (exactly one slash).
fn valid_owner_repo(a: &str) -> bool {
    let Some((owner, repo)) = a.split_once('/') else {
        return false;
    };
    if repo.is_empty() || repo.contains('/') {
        return false;
    }
    valid_owner(owner)
        && repo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// SSH scp-like git URL: `[A-Za-z0-9._-]+@[^:]+:.+` with an optional `#ref`.
/// Returns `(url, ref?)` (the `#ref` split off the url), else `None`.
fn match_ssh_git(t: &str) -> Option<(String, Option<String>)> {
    let (left, git_ref) = match t.split_once('#') {
        Some((l, r)) if !r.is_empty() => (l, Some(r.to_string())),
        Some((l, _)) => (l, None),
        None => (t, None),
    };
    let at = left.find('@')?;
    let user = &left[..at];
    if user.is_empty()
        || !user
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || c == b'-')
    {
        return None;
    }
    let rest = &left[at + 1..];
    let colon = rest.find(':')?;
    if colon == 0 || rest[colon + 1..].is_empty() {
        return None; // host non-empty, path non-empty
    }
    Some((left.to_string(), git_ref))
}

/// The `(host, pathname)` of an HTTP(S) URL (userinfo/`:port`/query/fragment
/// stripped from the host; pathname stops at `?`/`#`).
fn http_host_path(url: &str) -> (String, String) {
    let after = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let hostport = &after[..end];
    let host = hostport.rsplit('@').next().unwrap_or(hostport);
    let host = host.split(':').next().unwrap_or(host);
    let rest = &after[end..];
    let path_end = rest.find(['?', '#']).unwrap_or(rest.len());
    (host.to_string(), rest[..path_end].to_string())
}

/// The known git host (binary `$m`): `github.com` after stripping `www.`.
fn is_github_host(host: &str) -> bool {
    let mut h = host;
    while let Some(rest) = h.strip_prefix("www.") {
        h = rest;
    }
    h == "github.com"
}

/// Is `host` a GitLab host? Oracle `nwo`.
///
/// GitLab is matched by suffix as well as exactly, because self-hosted
/// instances live on `gitlab.<company>.com`-shaped names the way GHE does.
fn is_gitlab_host(host: &str) -> bool {
    let mut h = host;
    while let Some(rest) = h.strip_prefix("www.") {
        h = rest;
    }
    h == "gitlab.com" || h.starts_with("gitlab.") || h.contains(".gitlab.")
}

/// Does this GitLab URL name a project we can clone? Oracle `I$e`'s GitLab arm.
///
/// GitLab nests projects under groups and subgroups, so `owner/repo` is a
/// FLOOR, not the shape. The oracle's guards, all of them load-bearing:
///
/// * `M.length >= 2` — at least `group/project`.
/// * `F === "/" + M.join("/")` — the path must round-trip exactly, so a URL
///   carrying percent-encoding or empty segments is not silently rewritten.
/// * no tab/newline/CR anywhere in the URL.
/// * every segment must decode.
/// * `U[0] !== "api"` — `/api/...` is the REST API, not a project.
/// * `!U.includes("-")` — GitLab's `/-/` separator introduces a sub-resource
///   (`/-/merge_requests/5`, `/-/tree/main`), so the URL names a page inside a
///   project rather than the project itself.
fn gitlab_path_is_project(url: &str, path: &str) -> bool {
    if url.contains(['\t', '\n', '\r']) {
        return false;
    }
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return false;
    }
    // Exact round-trip: rebuilding the path from its segments must reproduce it.
    let trimmed = path.trim_end_matches('/');
    if trimmed != format!("/{}", segments.join("/")) {
        return false;
    }
    let decoded: Vec<String> = match segments
        .iter()
        .map(|s| percent_decode_segment(s))
        .collect::<Option<Vec<_>>>()
    {
        Some(d) => d,
        None => return false,
    };
    if decoded.first().is_some_and(|s| s == "api") {
        return false;
    }
    !decoded.iter().any(|s| s == "-")
}

/// Percent-decode one path segment, or `None` when it is not valid UTF-8 —
/// the oracle's `try{decodeURIComponent(seg)}catch{return null}`.
fn percent_decode_segment(segment: &str) -> Option<String> {
    if !segment.contains('%') {
        return Some(segment.to_string());
    }
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // `decodeURIComponent` throws on a truncated or non-hex escape.
            let hex = bytes.get(i + 1..i + 3)?;
            let text = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(text, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// pathname `^/[^/]+/[^/]+` — at least an `owner/repo` pair.
fn path_has_owner_repo(path: &str) -> bool {
    let mut segs = path.trim_start_matches('/').split('/');
    matches!((segs.next(), segs.next()), (Some(a), Some(b)) if !a.is_empty() && !b.is_empty())
}

/// Classify a `plugin marketplace add` source string, 1:1 with the binary's
/// `Idr` order: SSH git → HTTP(S) → local (`./ ../ / ~`) → GitHub shorthand →
/// unclassifiable. `Err` is a fully-formed (pre-"Adding marketplace…") error.
fn classify_source(source: &str) -> Result<Source, String> {
    let t = source.trim();

    // 1. SSH scp-like git URL.
    if let Some((url, git_ref)) = match_ssh_git(t) {
        return Ok(Source::Git { url, git_ref });
    }

    // 2. HTTP(S): a `.git` / `/_git/` URL (any host) is a git clone; a
    //    `github.com/owner/repo` URL is a git clone with `.git` appended;
    //    anything else is a hosted `marketplace.json` (`url`).
    if t.starts_with("http://") || t.starts_with("https://") {
        let (base, git_ref) = match t.split_once('#') {
            Some((b, r)) if !r.is_empty() => (b.to_string(), Some(r.to_string())),
            _ => (t.trim_end_matches('#').to_string(), None),
        };
        if base.ends_with(".git") || base.contains("/_git/") {
            return Ok(Source::Git { url: base, git_ref });
        }
        let (host, path) = http_host_path(&base);
        if is_github_host(&host) && path_has_owner_repo(&path) {
            let url = if base.ends_with(".git") {
                base
            } else {
                format!("{base}.git")
            };
            return Ok(Source::Git { url, git_ref });
        }
        // GitLab (oracle `I$e`'s second host arm). Without this a bare
        // `https://gitlab.com/group/project` fell through to `Source::Url` and
        // was fetched as a hosted `marketplace.json`, which it is not.
        if is_gitlab_host(&host) && gitlab_path_is_project(&base, &path) {
            let trimmed = base.trim_end_matches('/');
            let url = if trimmed.ends_with(".git") {
                trimmed.to_string()
            } else {
                format!("{trimmed}.git")
            };
            return Ok(Source::Git { url, git_ref });
        }
        return Ok(Source::Url { url: base });
    }

    // 3. Local path (explicit `./ ../ / ~` prefix — matching the binary, which
    //    does NOT treat a bare relative name as a path). Existence + kind are
    //    resolved via the filesystem; only a directory is supported here (a
    //    `.json` file source is a residual — see module residuals).
    if t.starts_with("./") || t.starts_with("../") || t.starts_with('/') || t.starts_with('~') {
        let abs = std::fs::canonicalize(source)
            .map_err(|_| format!("✘ Path does not exist: {source}"))?;
        if !abs.is_dir() {
            return Err(format!("✘ Path does not exist: {source}"));
        }
        return Ok(Source::Directory(abs));
    }

    // 4. GitHub `owner/repo` shorthand (contains `/`, not `@`-prefixed, no `:`).
    if t.contains('/') && !t.starts_with('@') {
        if t.contains(':') {
            return Err(invalid_source_format());
        }
        let (repo, git_ref) = {
            let idx = t.find(['#', '@']);
            match idx {
                Some(i) => {
                    let r = &t[i + 1..];
                    (
                        t[..i].to_string(),
                        if r.is_empty() {
                            None
                        } else {
                            Some(r.to_string())
                        },
                    )
                }
                None => (t.to_string(), None),
            }
        };
        if !valid_owner_repo(&repo) {
            return Err(invalid_shorthand(t));
        }
        return Ok(Source::Github { repo, git_ref });
    }

    // 5. Unclassifiable.
    Err(invalid_source_format())
}

/// The settings / registry `source` sub-object for a classified source.
///
/// `sparse` (oracle `sparsePaths`, `--sparse <paths...>`) is recorded only for
/// `github`/`git` — the caller has already rejected it for every other kind
/// with the oracle's exact guard error, so a non-empty `sparse` here is only
/// ever reached for those two.
fn source_object(src: &Source, sparse: &[String]) -> Value {
    let mut o = Map::new();
    match src {
        Source::Directory(p) => {
            o.insert("source".to_string(), Value::String("directory".to_string()));
            o.insert("path".to_string(), Value::String(p.display().to_string()));
        }
        Source::Github { repo, git_ref } => {
            o.insert("source".to_string(), Value::String("github".to_string()));
            o.insert("repo".to_string(), Value::String(repo.clone()));
            if let Some(r) = git_ref {
                o.insert("ref".to_string(), Value::String(r.clone()));
            }
        }
        Source::Git { url, git_ref } => {
            o.insert("source".to_string(), Value::String("git".to_string()));
            o.insert("url".to_string(), Value::String(url.clone()));
            if let Some(r) = git_ref {
                o.insert("ref".to_string(), Value::String(r.clone()));
            }
        }
        Source::Url { url } => {
            o.insert("source".to_string(), Value::String("url".to_string()));
            o.insert("url".to_string(), Value::String(url.clone()));
        }
    }
    if !sparse.is_empty() && matches!(src, Source::Github { .. } | Source::Git { .. }) {
        o.insert(
            "sparsePaths".to_string(),
            Value::Array(sparse.iter().cloned().map(Value::String).collect()),
        );
    }
    Value::Object(o)
}

/// `plugin marketplace add <source> [--scope] [--sparse]`.
///
/// Classifies `<source>` (directory / GitHub-shorthand / git-URL / hosted-URL,
/// 1:1 with the binary `Idr`), then writes BOTH the resolved registry entry
/// (`known_marketplaces.json`) and the per-scope `extraKnownMarketplaces`
/// declaration. If the registry already has the name it only (re)writes the
/// scope declaration and reports "already on disk".
///
/// - **directory** (verified 1:1): read + validate the local
///   `<dir>/.lingxi-plugin/marketplace.json` (requires `name` + `owner` object).
/// - **github / git** (network): clone the marketplace repo into
///   `<plugins>/marketplaces/<name>/` via `MarketplaceManager::resolve_index_via_git`,
///   taking the marketplace `name` from the cloned catalog and setting
///   `installLocation` to the clone dir.
/// - **url**: a hosted `marketplace.json` fetched over HTTPS into an atomic
///   local catalog directory.
pub fn run_add(
    source: &str,
    scope: Option<&str>,
    sparse: &[String],
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
) -> Result<String, String> {
    let policy_source = preflight_source_identity(source, cwd)?;
    plugin_policy::ensure_marketplace_source_preflight(&policy_source)
        .map_err(|reason| format!("Adding marketplace…✘ Failed to add marketplace: {reason}"))?;
    // Classification (incl. local existence) is checked FIRST — before scope,
    // and before any "Adding marketplace…" progress line — matching the binary.
    let classified = classify_source(source)?;
    // Oracle `qn`: the scope is validated FIRST
    // (`if(w=i.scope??"user",w!=="user"&&w!=="project"&&w!=="local")return …`),
    // and only then does the `--sparse` kind guard run. With both wrong, the
    // caller must see the scope error.
    let target = match scope {
        Some(s) => parse_scope_str(s).ok_or_else(|| market_invalid_scope(s))?,
        None => WritableScope::User,
    };
    if !sparse.is_empty() && !matches!(classified, Source::Github { .. } | Source::Git { .. }) {
        return Err(format!(
            "✘ --sparse is only supported for github and git marketplace sources (got: {})",
            source_kind(&classified)
        ));
    }

    match classified {
        Source::Directory(abs) => add_directory(&abs, target, plugins_dir, home, cwd),
        remote => add_remote(&remote, sparse, target, plugins_dir, home, cwd),
    }
}

/// The local-directory add path (unchanged behaviour): validate the manifest,
/// then write the scope declaration + registry entry.
fn add_directory(
    abs: &Path,
    target: WritableScope,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
) -> Result<String, String> {
    // From here the "Adding marketplace…" progress prefix is part of the line.
    let manifest_path = abs
        .join(branding::PLUGIN_MANIFEST_DIR)
        .join("marketplace.json");
    let raw = std::fs::read_to_string(&manifest_path).map_err(|_| {
        format!(
            "Adding marketplace…✘ Failed to add marketplace: Marketplace file not found at {}",
            manifest_path.display()
        )
    })?;
    let manifest: Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Adding marketplace…✘ Failed to add marketplace: Failed to parse marketplace file at {}: {e}",
            manifest_path.display()
        )
    })?;
    let name = manifest
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!(
                "Adding marketplace…✘ Failed to add marketplace: Failed to parse marketplace file at {}: Invalid schema: missing required field 'name'",
                manifest_path.display()
            )
        })?
        .to_string();
    if !manifest.get("owner").is_some_and(Value::is_object) {
        return Err(format!(
            "Adding marketplace…✘ Failed to add marketplace: Failed to parse marketplace file at {}: Invalid schema: {} owner: Invalid input: expected object, received undefined",
            manifest_path.display(),
            manifest_path.display()
        ));
    }
    // §8: the name gate previously existed only on the authoring path
    // (`plugin tag`/`plugin init`) — marketplace ingestion never called it, so
    // a local `marketplace.json` naming itself e.g. `claude-official` (or
    // carrying a bidi-spoofed name) registered without complaint.
    plugin::validate_marketplace_name(&name)
        .map_err(|reason| format!("Adding marketplace…✘ Failed to add marketplace: {reason}"))?;
    let identity = MarketplaceSourceIdentity::Directory {
        path: abs.display().to_string(),
    };
    plugin_policy::ensure_marketplace_source_allowed(Some(&name), Some(&identity))
        .map_err(|reason| format!("Adding marketplace…✘ Failed to add marketplace: {reason}"))?;

    let source_value = source_object(&Source::Directory(abs.to_path_buf()), &[]);
    write_marketplace(
        &name,
        &source_value,
        &abs.display().to_string(),
        target,
        plugins_dir,
        home,
        cwd,
    )
}

/// The network add path (github / git clone / hosted URL). Stages the catalog,
/// takes the marketplace `name` from it, and records `installLocation` = the
/// clone dir.
fn add_remote(
    remote: &Source,
    sparse: &[String],
    target: WritableScope,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
) -> Result<String, String> {
    // Determine the clone URL + a provisional name used only for a unique
    // staging path. Publication waits for the catalog's name-aware policy.
    let (clone_url, hint, git_ref) = match remote {
        Source::Github { repo, git_ref } => (
            format!("https://github.com/{repo}.git"),
            repo.clone(),
            git_ref.as_deref(),
        ),
        Source::Git { url, git_ref } => (url.clone(), url_repo_hint(url), git_ref.as_deref()),
        Source::Url { url } => {
            let (name, staged_catalog) = fetch_hosted_marketplace(plugins_dir, url)
                .map_err(|e| format!("Adding marketplace…✘ Failed to add marketplace: {e}"))?;
            // §8: the hosted catalog's declared `name` is fully attacker-
            // controlled (this fetch has no relationship to `<source>`'s own
            // host); validate it before it can be registered.
            if let Err(reason) = plugin::validate_marketplace_name(&name) {
                let _ = std::fs::remove_dir_all(&staged_catalog);
                return Err(format!(
                    "Adding marketplace…✘ Failed to add marketplace: {reason}"
                ));
            }
            let identity = source_identity(remote);
            if let Err(reason) =
                plugin_policy::ensure_marketplace_source_allowed(Some(&name), Some(&identity))
            {
                let _ = std::fs::remove_dir_all(&staged_catalog);
                return Err(format!(
                    "Adding marketplace…✘ Failed to add marketplace: {reason}"
                ));
            }
            let source_value = source_object(remote, &[]);
            return publish_and_write_marketplace(
                &name,
                &source_value,
                &staged_catalog,
                target,
                plugins_dir,
                home,
                cwd,
            );
        }
        Source::Directory(_) => unreachable!("add_remote called with a directory source"),
    };
    if plugin_policy::blocked_marketplaces().contains(&hint) {
        return Err(format!(
            "Adding marketplace…✘ Failed to add marketplace: Marketplace '{hint}' is blocked by managed settings"
        ));
    }

    // From here the "Adding marketplace…" progress prefix is part of the line.
    let (name, staged_clone) =
        clone_marketplace(plugins_dir, &clone_url, &hint, git_ref, sparse)
            .map_err(|e| format!("Adding marketplace…✘ Failed to add marketplace: {e}"))?;
    // §8: the cloned catalog's declared `name` is attacker-controlled the
    // moment `<source>` names a repo the caller doesn't own; validate it
    // before it can be registered.
    if let Err(reason) = plugin::validate_marketplace_name(&name) {
        let _ = std::fs::remove_dir_all(&staged_clone);
        return Err(format!(
            "Adding marketplace…✘ Failed to add marketplace: {reason}"
        ));
    }
    let identity = source_identity(remote);
    if let Err(reason) =
        plugin_policy::ensure_marketplace_source_allowed(Some(&name), Some(&identity))
    {
        let _ = std::fs::remove_dir_all(&staged_clone);
        return Err(format!(
            "Adding marketplace…✘ Failed to add marketplace: {reason}"
        ));
    }

    let source_value = source_object(remote, sparse);
    publish_and_write_marketplace(
        &name,
        &source_value,
        &staged_clone,
        target,
        plugins_dir,
        home,
        cwd,
    )
}

const MAX_HOSTED_MARKETPLACE_BYTES: usize = 5 * 1024 * 1024;

/// Fetch a hosted marketplace catalog without following redirects, validate
/// its public shape, and leave it in a unique same-parent staging directory.
/// The caller applies the name-aware policy before publishing that directory.
fn fetch_hosted_marketplace(plugins_dir: &Path, url: &str) -> Result<(String, PathBuf), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("Invalid marketplace URL: {e}"))?;
    if parsed.scheme() != "https" {
        return Err("Hosted marketplace URLs must use HTTPS".to_string());
    }
    let plugins_dir = plugins_dir.to_path_buf();
    let url = url.to_string();
    std::thread::spawn(move || -> Result<(String, PathBuf), String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("Failed to initialize marketplace download: {e}"))?;
        runtime.block_on(async move {
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|e| format!("Failed to initialize marketplace download: {e}"))?;
            let mut response = client
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("Failed to download marketplace: {e}"))?;
            if !response.status().is_success() {
                return Err(format!(
                    "Failed to download marketplace: HTTP {}",
                    response.status()
                ));
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_HOSTED_MARKETPLACE_BYTES as u64)
            {
                return Err("Hosted marketplace exceeds the 5 MiB limit".to_string());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| format!("Failed to download marketplace: {e}"))?
            {
                if bytes.len().saturating_add(chunk.len()) > MAX_HOSTED_MARKETPLACE_BYTES {
                    return Err("Hosted marketplace exceeds the 5 MiB limit".to_string());
                }
                bytes.extend_from_slice(&chunk);
            }
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|e| format!("Invalid marketplace schema: {e}"))?;
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| "Invalid marketplace schema: missing required field 'name'".to_string())?
                .to_string();
            if !value.get("owner").is_some_and(Value::is_object)
                || !value.get("plugins").is_some_and(Value::is_array)
            {
                return Err(
                    "Invalid marketplace schema: owner must be an object and plugins must be an array"
                        .to_string(),
                );
            }

            let parent = plugins_dir.join("marketplaces");
            std::fs::create_dir_all(&parent)
                .map_err(|e| format!("Failed to create marketplace cache: {e}"))?;
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let temp = parent.join(format!(".hosted-{}-{nonce}", std::process::id()));
            let manifest_dir = temp.join(branding::PLUGIN_MANIFEST_DIR);
            std::fs::create_dir_all(&manifest_dir)
                .map_err(|e| format!("Failed to create marketplace cache: {e}"))?;
            if let Err(error) = std::fs::write(manifest_dir.join("marketplace.json"), &bytes) {
                let _ = std::fs::remove_dir_all(&temp);
                return Err(format!("Failed to cache marketplace: {error}"));
            }
            Ok((name, temp))
        })
    })
    .join()
    .map_err(|_| "Marketplace download worker panicked".to_string())?
}

/// A provisional clone-dir name derived from a git URL's last path segment
/// (`.git` stripped). It is used only to label the transient staging path.
fn url_repo_hint(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
    last.strip_suffix(".git").unwrap_or(last).to_string()
}

/// Sanitize one clone-dir path segment exactly as the plugin crate's
/// (`pub`) `discovery::sanitize_segment` does for marketplace names:
/// replace any char outside `[A-Za-z0-9\-_]` with `-`, and collapse an
/// empty / `.` / `..` result to `-`.
fn sanitize_segment(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if mapped.is_empty() || mapped == "." || mapped == ".." {
        "-".to_string()
    } else {
        mapped
    }
}

/// Clone (and parse) a marketplace git repo via the plugin crate's
/// `MarketplaceManager::resolve_index_via_git` — run on a dedicated thread with
/// its own current-thread runtime so it is safe to call from the async CLI
/// dispatcher without nesting runtimes. Returns `(marketplace_name, clone_dir)`,
/// where the clone remains in a unique staging directory until the caller has
/// applied the catalog's name-aware managed policy.
/// Confine a freshly-cloned marketplace working tree to `sparse_paths` (the
/// oracle's cone-mode `sparsePaths`, e.g. `[".claude-plugin", "plugins"]`):
/// every top-level directory NOT named by one of `sparse_paths`' first path
/// segments is removed; top-level files and `.git` are always kept (cone mode
/// keeps root-listed files). This narrows the WORKING TREE to the same
/// result cone-mode sparse-checkout produces; unlike the oracle it does not
/// reduce clone bandwidth (no safe sparse-checkout binding is available over
/// this crate's vendored-libgit2 transport — see the `MarketplaceManager` doc).
///
/// A `sparsePaths` list that omits the directory actually holding
/// `marketplace.json` prunes the manifest away too, exactly as a real
/// cone-mode sparse-checkout would — this is a user configuration error, not
/// a bug here.
fn prune_to_sparse_paths(clone_dir: &Path, sparse_paths: &[String]) -> Result<(), String> {
    if sparse_paths.is_empty() {
        return Ok(());
    }
    let keep: std::collections::HashSet<&str> = sparse_paths
        .iter()
        .map(|p| p.split('/').next().unwrap_or(p.as_str()))
        .collect();
    let entries = std::fs::read_dir(clone_dir)
        .map_err(|e| format!("Failed to apply --sparse to the clone: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to apply --sparse to the clone: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == ".git" || keep.contains(name) {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            std::fs::remove_dir_all(entry.path())
                .map_err(|e| format!("Failed to apply --sparse to the clone: {e}"))?;
        }
    }
    Ok(())
}

fn clone_marketplace(
    plugins_dir: &Path,
    clone_url: &str,
    hint: &str,
    git_ref: Option<&str>,
    sparse: &[String],
) -> Result<(String, PathBuf), String> {
    let plugins_dir = plugins_dir.to_path_buf();
    let url = clone_url.to_string();
    let hint = hint.to_string();
    let git_ref = git_ref.map(ToOwned::to_owned);
    let sparse = sparse.to_vec();
    std::thread::spawn(move || -> Result<(String, PathBuf), String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let staging_hint = format!(
            ".incoming-{}-{}-{nonce}",
            sanitize_segment(&hint),
            std::process::id()
        );
        let mgr = plugin::MarketplaceManager::new(plugins_dir.clone());
        match rt.block_on(mgr.resolve_index_via_git_ref(&url, &staging_hint, git_ref.as_deref())) {
            Ok((index, clone_dir)) => {
                if let Err(error) = prune_to_sparse_paths(&clone_dir, &sparse) {
                    let _ = std::fs::remove_dir_all(&clone_dir);
                    return Err(error);
                }
                Ok((index.name, clone_dir))
            }
            Err(error) => {
                let staged = plugins_dir
                    .join("marketplaces")
                    .join(sanitize_segment(&staging_hint));
                let _ = std::fs::remove_dir_all(staged);
                Err(error)
            }
        }
    })
    .join()
    .map_err(|_| "Failed to clone marketplace repository: worker thread panicked".to_string())?
}

struct PublishedMarketplace {
    destination: PathBuf,
    backup: Option<PathBuf>,
}

impl PublishedMarketplace {
    fn commit(self) {
        if let Some(backup) = self.backup {
            let _ = std::fs::remove_dir_all(backup);
        }
    }

    fn rollback(self) {
        let _ = std::fs::remove_dir_all(&self.destination);
        if let Some(backup) = self.backup {
            let _ = std::fs::rename(backup, self.destination);
        }
    }
}

/// Atomically replace one published marketplace cache after all policy gates.
fn publish_marketplace_cache(
    plugins_dir: &Path,
    name: &str,
    staged: &Path,
) -> Result<PublishedMarketplace, String> {
    let parent = plugins_dir.join("marketplaces");
    let canonical_parent = std::fs::canonicalize(&parent)
        .map_err(|error| format!("Failed to resolve marketplace cache: {error}"))?;
    let canonical_staged = std::fs::canonicalize(staged)
        .map_err(|error| format!("Failed to resolve staged marketplace: {error}"))?;
    if canonical_staged.parent() != Some(canonical_parent.as_path()) {
        return Err("Refusing to publish a marketplace outside its cache root".to_string());
    }
    let destination = parent.join(sanitize_segment(name));
    if destination == canonical_staged {
        return Err("Marketplace staging path collides with its destination".to_string());
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup = destination
        .exists()
        .then(|| parent.join(format!(".backup-{}-{nonce}", std::process::id())));
    if let Some(backup) = &backup {
        std::fs::rename(&destination, backup)
            .map_err(|error| format!("Failed to stage existing marketplace cache: {error}"))?;
    }
    if let Err(error) = std::fs::rename(&canonical_staged, &destination) {
        if let Some(backup) = &backup {
            let _ = std::fs::rename(backup, &destination);
        }
        return Err(format!("Failed to publish marketplace cache: {error}"));
    }
    Ok(PublishedMarketplace {
        destination,
        backup,
    })
}

fn publish_and_write_marketplace(
    name: &str,
    source_value: &Value,
    staged: &Path,
    target: WritableScope,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
) -> Result<String, String> {
    let _lock = lock_marketplace_state(plugins_dir)
        .map_err(|error| format!("Adding marketplace…✘ Failed to add marketplace: {error}"))?;
    if load_registry(plugins_dir).contains_key(name) {
        let result = write_marketplace_unlocked(
            name,
            source_value,
            &staged.display().to_string(),
            target,
            plugins_dir,
            home,
            cwd,
        );
        let _ = std::fs::remove_dir_all(staged);
        return result;
    }
    let published = match publish_marketplace_cache(plugins_dir, name, staged) {
        Ok(published) => published,
        Err(error) => {
            let _ = std::fs::remove_dir_all(staged);
            return Err(format!(
                "Adding marketplace…✘ Failed to add marketplace: {error}"
            ));
        }
    };
    let install_location = published.destination.display().to_string();
    match write_marketplace_unlocked(
        name,
        source_value,
        &install_location,
        target,
        plugins_dir,
        home,
        cwd,
    ) {
        Ok(message) => {
            published.commit();
            Ok(message)
        }
        Err(error) => {
            published.rollback();
            Err(error)
        }
    }
}

/// Shared writer for both add paths: write the per-scope declaration, then the
/// resolved registry entry (or report "already on disk").
fn write_marketplace(
    name: &str,
    source_value: &Value,
    install_location: &str,
    target: WritableScope,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
) -> Result<String, String> {
    let _lock = lock_marketplace_state(plugins_dir)
        .map_err(|error| format!("Adding marketplace…✘ Failed to add marketplace: {error}"))?;
    write_marketplace_unlocked(
        name,
        source_value,
        install_location,
        target,
        plugins_dir,
        home,
        cwd,
    )
}

fn write_marketplace_unlocked(
    name: &str,
    source_value: &Value,
    install_location: &str,
    target: WritableScope,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
) -> Result<String, String> {
    let settings_path = scope_path(target, home, cwd);
    let previous_settings = std::fs::read(&settings_path).ok();
    let previous_registry = platform_api::rooted_fs::read_to_string_limited(
        plugins_dir,
        Path::new("known_marketplaces.json"),
        16 * 1024 * 1024,
    )
    .ok()
    .map(String::into_bytes);
    // Per-scope declaration (settings.extraKnownMarketplaces[name] = {source}).
    let mut extra = read_extra(target, home, cwd);
    extra.insert(
        name.to_string(),
        serde_json::json!({ "source": source_value.clone() }),
    );
    if let Err(error) = write_extra(target, home, cwd, extra) {
        restore_snapshot(&settings_path, previous_settings.as_deref());
        return Err(format!(
            "Adding marketplace…✘ Failed to add marketplace: {error}"
        ));
    }

    // Registry (resolved) — only written when the name is not already on disk.
    let mut registry = load_registry(plugins_dir);
    if registry.contains_key(name) {
        return Ok(format!(
            "Adding marketplace…✔ Marketplace '{name}' already on disk — declared in {} settings",
            scope_label(target)
        ));
    }
    registry.insert(
        name.to_string(),
        serde_json::json!({
            "source": source_value.clone(),
            "installLocation": install_location,
            "lastUpdated": iso_now(),
        }),
    );
    if let Err(error) = write_registry(plugins_dir, &registry) {
        restore_snapshot(&settings_path, previous_settings.as_deref());
        restore_registry_snapshot(plugins_dir, previous_registry.as_deref());
        return Err(format!(
            "Adding marketplace…✘ Failed to add marketplace: {error}"
        ));
    }
    Ok(format!(
        "Adding marketplace…✔ Successfully added marketplace: {name} (declared in {} settings)",
        scope_label(target)
    ))
}

fn lock_marketplace_state(plugins_dir: &Path) -> Result<platform_api::RootedFileLock, String> {
    std::fs::create_dir_all(plugins_dir)
        .map_err(|error| format!("Failed to create plugin state root: {error}"))?;
    platform_api::rooted_fs::lock_exclusive(
        plugins_dir,
        Path::new(".marketplace.lock"),
        platform_api::rooted_fs::PRIVATE_DIR_MODE,
        platform_api::rooted_fs::PRIVATE_FILE_MODE,
    )
    .map_err(|error| format!("Failed to lock marketplace state: {error}"))
}

fn restore_snapshot(path: &Path, snapshot: Option<&[u8]>) {
    match snapshot {
        Some(bytes) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, bytes);
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn restore_registry_snapshot(plugins_dir: &Path, snapshot: Option<&[u8]>) {
    let result = match snapshot {
        Some(bytes) => platform_api::rooted_fs::atomic_write(
            plugins_dir,
            Path::new("known_marketplaces.json"),
            bytes,
            platform_api::AtomicWriteOptions::default(),
        ),
        None => {
            platform_api::rooted_fs::remove_file(plugins_dir, Path::new("known_marketplaces.json"))
        }
    };
    if let Err(error) = result {
        tracing::warn!(%error, "failed to restore marketplace registry");
    }
}

/// `plugin marketplace remove <name> [--scope]`.
///
/// Removes the per-scope `extraKnownMarketplaces` declaration (from the given
/// scope, or every scope). When no scope declares the name anymore, drops the
/// resolved registry entry too. Not-configured → the `not found` error.
pub fn run_remove(
    name: &str,
    scope: Option<&str>,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
) -> Result<String, String> {
    let requested = match scope {
        Some(s) => Some(parse_scope_str(s).ok_or_else(|| market_invalid_scope(s))?),
        None => None,
    };
    let _lock = lock_marketplace_state(plugins_dir)?;
    let declaring = declaring_scopes(name, home, cwd);
    let targets: Vec<WritableScope> = match requested {
        Some(s) => {
            if declaring.contains(&s) {
                vec![s]
            } else {
                vec![]
            }
        }
        None => declaring.clone(),
    };
    if targets.is_empty() {
        // Nothing declared at the target — also consider a stray registry entry.
        if requested.is_none() && load_registry(plugins_dir).contains_key(name) {
            // Registry-only entry (no declaration): drop it and report success.
            let mut registry = load_registry(plugins_dir);
            registry.remove(name);
            write_registry(plugins_dir, &registry)?;
            return Ok(format!("✔ Successfully removed marketplace: {name}"));
        }
        return Err(format!(
            "✘ Failed to remove marketplace: Marketplace '{name}' not found"
        ));
    }

    for s in &targets {
        let mut extra = read_extra(*s, home, cwd);
        extra.remove(name);
        write_extra(*s, home, cwd, extra)?;
    }

    // Drop the resolved registry entry when no scope declares it anymore.
    if declaring_scopes(name, home, cwd).is_empty() {
        let mut registry = load_registry(plugins_dir);
        if registry.remove(name).is_some() {
            write_registry(plugins_dir, &registry)?;
        }
    }

    Ok(match requested {
        Some(s) => format!(
            "✔ Successfully removed marketplace: {name} (from {} settings)",
            scope_label(s)
        ),
        None => format!("✔ Successfully removed marketplace: {name}"),
    })
}

/// `plugin marketplace update [name]`.
///
/// Refreshes the registry `lastUpdated` (re-validating a local-directory
/// source's manifest); with no name updates every configured marketplace.
pub fn run_update(
    name: Option<&str>,
    plugins_dir: &Path,
    _home: &Path,
    _cwd: &Path,
) -> Result<String, String> {
    let _lock = lock_marketplace_state(plugins_dir)?;
    let mut registry = load_registry(plugins_dir);

    if let Some(name) = name {
        let source = registry
            .get(name)
            .and_then(MarketplaceSourceIdentity::from_value);
        plugin_policy::ensure_marketplace_source_allowed(Some(name), source.as_ref()).map_err(
            |reason| {
                format!(
                    "Updating marketplace: {name}...✘ Failed to update marketplace(s): {reason}"
                )
            },
        )?;
        if !registry.contains_key(name) {
            let available: Vec<&str> = registry.keys().map(String::as_str).collect();
            return Err(format!(
                "Updating marketplace: {name}...✘ Failed to update marketplace(s): Marketplace '{name}' not found. Available marketplaces: {}",
                available.join(", ")
            ));
        }
        let is_dir = registry
            .get(name)
            .map(source_of)
            .map(|s| str_field(&s, "source") == "directory")
            .unwrap_or(false);
        if let Some(entry) = registry.get_mut(name).and_then(Value::as_object_mut) {
            entry.insert("lastUpdated".to_string(), Value::String(iso_now()));
        }
        write_registry(plugins_dir, &registry)?;
        let validating = if is_dir {
            "Validating local marketplace\n"
        } else {
            ""
        };
        return Ok(format!(
            "Updating marketplace: {name}...{validating}✔ Successfully updated marketplace: {name}"
        ));
    }

    let count = registry.len();
    for (marketplace, entry) in &registry {
        let source = MarketplaceSourceIdentity::from_value(entry);
        plugin_policy::ensure_marketplace_source_allowed(Some(marketplace), source.as_ref())
            .map_err(|reason| {
                format!(
                    "Updating {count} marketplace(s)...✘ Failed to update marketplace(s): {reason}"
                )
            })?;
    }
    for entry in registry.values_mut() {
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("lastUpdated".to_string(), Value::String(iso_now()));
        }
    }
    write_registry(plugins_dir, &registry)?;
    Ok(format!(
        "Updating {count} marketplace(s)...✔ Successfully updated {count} marketplace(s)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Env {
        _tmp: tempfile::TempDir,
        plugins: PathBuf,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        Env { _tmp: tmp, plugins }
    }

    fn write_registry(e: &Env, v: &Value) {
        std::fs::write(
            e.plugins.join("known_marketplaces.json"),
            serde_json::to_string_pretty(v).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn empty_registry_human_and_json() {
        let e = env();
        assert_eq!(run_list(&e.plugins, false), "No marketplaces configured");
        assert_eq!(run_list(&e.plugins, true), "[]");
    }

    #[test]
    fn missing_registry_file_is_empty() {
        let e = env();
        std::fs::remove_dir_all(&e.plugins).unwrap();
        assert_eq!(run_list(&e.plugins, false), "No marketplaces configured");
    }

    #[test]
    fn directory_human_matches_oracle() {
        let e = env();
        write_registry(
            &e,
            &json!({
                "mymkt": {
                    "source": {"source": "directory", "path": "/abs/mymkt"},
                    "installLocation": "/abs/mymkt",
                    "lastUpdated": "2026-07-04T10:28:13.751Z"
                }
            }),
        );
        assert_eq!(
            run_list(&e.plugins, false),
            "Configured marketplaces:\n\n  ❯ mymkt\n    Source: Directory (/abs/mymkt)"
        );
    }

    #[test]
    fn directory_json_matches_oracle() {
        let e = env();
        write_registry(
            &e,
            &json!({
                "mymkt": {
                    "source": {"source": "directory", "path": "/abs/mymkt"},
                    "installLocation": "/abs/mymkt",
                    "lastUpdated": "2026-07-04T10:28:13.751Z"
                }
            }),
        );
        let expected = serde_json::to_string_pretty(&json!([{
            "name": "mymkt",
            "source": "directory",
            "path": "/abs/mymkt",
            "installLocation": "/abs/mymkt"
        }]))
        .unwrap();
        assert_eq!(run_list(&e.plugins, true), expected);
    }

    #[test]
    fn git_and_github_and_url_human_render() {
        let e = env();
        write_registry(
            &e,
            &json!({
                "gh":  {"source": {"source": "github", "repo": "acme/plugins", "ref": "v2"}},
                "g2":  {"source": {"source": "git", "url": "https://x/y.git"}},
                "web": {"source": {"source": "url", "url": "https://x/cat.json"}}
            }),
        );
        // Registry order is insertion order (serde_json preserve_order).
        assert_eq!(
            run_list(&e.plugins, false),
            "Configured marketplaces:\n\n  \
             ❯ gh\n    Source: GitHub (acme/plugins@v2)\n  \
             ❯ g2\n    Source: Git (https://x/y.git)\n  \
             ❯ web\n    Source: URL (https://x/cat.json)"
        );
    }

    /// A home + project + plugins triple with a local marketplace fixture dir.
    struct FullEnv {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        cwd: PathBuf,
        plugins: PathBuf,
        market: PathBuf,
    }

    /// Build a fixture with a valid `<market>/.lingxi-plugin/marketplace.json`
    /// (name = `mymkt`, has the required `owner` object).
    fn full_env() -> FullEnv {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("proj");
        let plugins = home.join("plugins");
        let market = tmp.path().join("mymkt");
        for d in [&home, &cwd, &plugins] {
            std::fs::create_dir_all(d).unwrap();
        }
        let mdir = market.join(branding::PLUGIN_MANIFEST_DIR);
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(
            mdir.join("marketplace.json"),
            r#"{"name":"mymkt","owner":{"name":"me"},"plugins":[]}"#,
        )
        .unwrap();
        FullEnv {
            _tmp: tmp,
            home,
            cwd,
            plugins,
            market,
        }
    }

    fn registry_of(e: &FullEnv) -> Value {
        let raw = std::fs::read_to_string(e.plugins.join("known_marketplaces.json")).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn add_directory_writes_registry_and_declaration() {
        let e = full_env();
        let src = e.market.to_string_lossy().to_string();
        let abs = std::fs::canonicalize(&e.market)
            .unwrap()
            .display()
            .to_string();
        let msg = run_add(&src, None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        assert_eq!(
            msg,
            "Adding marketplace…✔ Successfully added marketplace: mymkt (declared in user settings)"
        );
        // Registry entry (resolved).
        let reg = registry_of(&e);
        assert_eq!(
            reg["mymkt"]["source"],
            json!({"source": "directory", "path": abs})
        );
        assert_eq!(reg["mymkt"]["installLocation"], json!(abs));
        assert!(reg["mymkt"]["lastUpdated"].is_string());
        // Per-scope declaration (user settings).
        let user: Value =
            serde_json::from_str(&std::fs::read_to_string(e.home.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            user["extraKnownMarketplaces"]["mymkt"],
            json!({"source": {"source": "directory", "path": abs}})
        );
    }

    /// §8: marketplace ingestion previously never called the name gate at
    /// all — a local `marketplace.json` naming itself after an official
    /// Anthropic catalog registered without complaint.
    #[test]
    fn add_directory_rejects_a_marketplace_json_impersonating_an_official_catalog() {
        let e = full_env();
        std::fs::write(
            e.market
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("marketplace.json"),
            r#"{"name":"anthropic-official","owner":{"name":"me"},"plugins":[]}"#,
        )
        .unwrap();
        let src = e.market.to_string_lossy().to_string();

        let err = run_add(&src, None, &[], &e.plugins, &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "Adding marketplace…✘ Failed to add marketplace: Marketplace name impersonates an \
             official Anthropic/Claude marketplace"
        );
        assert!(
            !e.plugins.join("known_marketplaces.json").exists(),
            "a rejected name must never reach the registry"
        );
    }

    #[test]
    fn add_rolls_back_scope_settings_when_registry_write_fails() {
        let e = full_env();
        let settings_path = e.home.join("settings.json");
        std::fs::write(&settings_path, r#"{"existing":true}"#).unwrap();
        let registry = e.plugins.join("known_marketplaces.json");
        std::fs::create_dir_all(&registry).unwrap();

        let error = run_add(
            &e.market.to_string_lossy(),
            None,
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .expect_err("registry directory must make the atomic write fail");

        assert!(error.contains("Failed to write"), "{error}");
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(settings_path).unwrap())
                .unwrap(),
            json!({"existing": true})
        );
    }

    #[test]
    fn published_marketplace_can_restore_the_previous_cache() {
        let e = full_env();
        let parent = e.plugins.join("marketplaces");
        let destination = parent.join("mymkt");
        let staged = parent.join(".incoming-test");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("old"), "old").unwrap();
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("new"), "new").unwrap();

        let published = publish_marketplace_cache(&e.plugins, "mymkt", &staged).unwrap();
        assert!(destination.join("new").exists());
        published.rollback();

        assert!(destination.join("old").exists());
        assert!(!destination.join("new").exists());
    }

    #[cfg(unix)]
    #[test]
    fn marketplace_registry_write_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let e = full_env();
        let outside = e._tmp.path().join("outside-marketplaces.json");
        std::fs::write(&outside, r#"{"sentinel":true}"#).unwrap();
        symlink(&outside, e.plugins.join("known_marketplaces.json")).unwrap();

        let error = run_add(
            &e.market.to_string_lossy(),
            None,
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .expect_err("root-confined marketplace registry must reject symlinks");

        assert!(error.contains("Failed to write"), "{error}");
        assert_eq!(
            std::fs::read_to_string(outside).unwrap(),
            r#"{"sentinel":true}"#
        );
    }

    #[test]
    fn add_second_scope_reports_already_on_disk() {
        let e = full_env();
        let src = e.market.to_string_lossy().to_string();
        run_add(&src, Some("user"), &[], &e.plugins, &e.home, &e.cwd).unwrap();
        let msg = run_add(&src, Some("project"), &[], &e.plugins, &e.home, &e.cwd).unwrap();
        assert_eq!(
            msg,
            "Adding marketplace…✔ Marketplace 'mymkt' already on disk — declared in project settings"
        );
        // Project declaration written.
        let project: Value = serde_json::from_str(
            &std::fs::read_to_string(e.cwd.join(".lingxi").join("settings.json")).unwrap(),
        )
        .unwrap();
        assert!(project["extraKnownMarketplaces"]["mymkt"].is_object());
    }

    #[test]
    fn additional_marketplaces_alias_is_read_when_canonical_key_is_absent() {
        let e = full_env();
        std::fs::write(
            e.home.join("settings.json"),
            r#"{
                "additionalMarketplaces": {
                    "alias": { "source": { "source": "directory", "path": "/tmp/alias" } }
                }
            }"#,
        )
        .unwrap();
        let extra = read_extra(WritableScope::User, &e.home, &e.cwd);
        assert_eq!(
            extra.get("alias"),
            Some(&json!({ "source": { "source": "directory", "path": "/tmp/alias" } }))
        );
    }

    #[test]
    fn extra_known_marketplaces_wins_over_additional_marketplaces_alias() {
        let e = full_env();
        std::fs::write(
            e.home.join("settings.json"),
            r#"{
                "extraKnownMarketplaces": {
                    "canonical": { "source": { "source": "directory", "path": "/tmp/canonical" } }
                },
                "additionalMarketplaces": {
                    "alias": { "source": { "source": "directory", "path": "/tmp/alias" } }
                }
            }"#,
        )
        .unwrap();
        let extra = read_extra(WritableScope::User, &e.home, &e.cwd);
        assert!(extra.get("canonical").is_some());
        assert!(extra.get("alias").is_none());
    }

    #[test]
    fn add_path_not_exist_errors_without_prefix() {
        let e = full_env();
        let err = run_add("/no/such/dir", None, &[], &e.plugins, &e.home, &e.cwd).unwrap_err();
        assert_eq!(err, "✘ Path does not exist: /no/such/dir");
    }

    #[test]
    fn add_missing_manifest_errors_with_prefix() {
        let e = full_env();
        let empty = e._tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let err = run_add(
            &empty.to_string_lossy(),
            None,
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();
        assert!(
            err.starts_with(
                "Adding marketplace…✘ Failed to add marketplace: Marketplace file not found at "
            ),
            "got: {err}"
        );
    }

    #[test]
    fn add_missing_owner_errors() {
        let e = full_env();
        let bad = e._tmp.path().join("bad");
        let mdir = bad.join(branding::PLUGIN_MANIFEST_DIR);
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(
            mdir.join("marketplace.json"),
            r#"{"name":"bad","plugins":[]}"#,
        )
        .unwrap();
        let err = run_add(
            &bad.to_string_lossy(),
            None,
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();
        assert!(
            err.contains("owner: Invalid input: expected object, received undefined"),
            "got: {err}"
        );
    }

    #[test]
    fn add_invalid_scope_errors() {
        let e = full_env();
        let err = run_add(
            &e.market.to_string_lossy(),
            Some("bogus"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();
        assert_eq!(err, "✘ Invalid scope 'bogus'. Use: user, project, or local");
    }

    #[test]
    fn remove_scoped_keeps_registry_when_still_declared() {
        let e = full_env();
        let src = e.market.to_string_lossy().to_string();
        run_add(&src, Some("user"), &[], &e.plugins, &e.home, &e.cwd).unwrap();
        run_add(&src, Some("project"), &[], &e.plugins, &e.home, &e.cwd).unwrap();
        let msg = run_remove("mymkt", Some("project"), &e.plugins, &e.home, &e.cwd).unwrap();
        assert_eq!(
            msg,
            "✔ Successfully removed marketplace: mymkt (from project settings)"
        );
        // Still declared in user → registry kept.
        assert!(registry_of(&e).get("mymkt").is_some());
    }

    #[test]
    fn remove_all_drops_registry() {
        let e = full_env();
        let src = e.market.to_string_lossy().to_string();
        run_add(&src, Some("user"), &[], &e.plugins, &e.home, &e.cwd).unwrap();
        let msg = run_remove("mymkt", None, &e.plugins, &e.home, &e.cwd).unwrap();
        assert_eq!(msg, "✔ Successfully removed marketplace: mymkt");
        assert_eq!(registry_of(&e), json!({}));
    }

    #[test]
    fn remove_not_configured_errors() {
        let e = full_env();
        let err = run_remove("ghost", None, &e.plugins, &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "✘ Failed to remove marketplace: Marketplace 'ghost' not found"
        );
    }

    #[test]
    fn update_named_directory_validates() {
        let e = full_env();
        let src = e.market.to_string_lossy().to_string();
        run_add(&src, None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        let msg = run_update(Some("mymkt"), &e.plugins, &e.home, &e.cwd).unwrap();
        assert_eq!(
            msg,
            "Updating marketplace: mymkt...Validating local marketplace\n✔ Successfully updated marketplace: mymkt"
        );
    }

    #[test]
    fn update_all_counts() {
        let e = full_env();
        let src = e.market.to_string_lossy().to_string();
        run_add(&src, None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        let msg = run_update(None, &e.plugins, &e.home, &e.cwd).unwrap();
        assert_eq!(
            msg,
            "Updating 1 marketplace(s)...✔ Successfully updated 1 marketplace(s)"
        );
    }

    // ---- source classification (pure, no network) ----

    #[test]
    fn classify_github_shorthand() {
        assert_eq!(
            classify_source("anthropics/claude-plugins-official").unwrap(),
            Source::Github {
                repo: "anthropics/claude-plugins-official".to_string(),
                git_ref: None
            }
        );
        // `#ref` and `@ref` both split off the ref.
        assert_eq!(
            classify_source("acme/plugins#v2").unwrap(),
            Source::Github {
                repo: "acme/plugins".to_string(),
                git_ref: Some("v2".to_string())
            }
        );
        assert_eq!(
            classify_source("acme/plugins@main").unwrap(),
            Source::Github {
                repo: "acme/plugins".to_string(),
                git_ref: Some("main".to_string())
            }
        );
    }

    #[test]
    fn classify_github_shorthand_invalid() {
        // Two slashes → not `owner/repo` → the shorthand error (has prefix `✘`).
        let err = classify_source("foo/bar/baz").unwrap_err();
        assert_eq!(
            err,
            "✘ 'foo/bar/baz' is not a valid GitHub owner/repo shorthand. For a git repo, use the full https:// clone URL from your host (typically ending in .git — some hosts like Azure DevOps omit it). For a hosted marketplace.json, use its https:// URL. For a local path, use ./ or an absolute path."
        );
    }

    #[test]
    fn classify_unclassifiable() {
        for bad in ["not a repo", "", "justtext"] {
            assert_eq!(
                classify_source(bad).unwrap_err(),
                "✘ Invalid marketplace source format. Try: owner/repo, https://..., or ./path"
            );
        }
        // A `/`-bearing string that also has `:` is unclassifiable (not github).
        assert_eq!(
            classify_source("foo/bar:baz").unwrap_err(),
            "✘ Invalid marketplace source format. Try: owner/repo, https://..., or ./path"
        );
    }

    #[test]
    fn classify_ssh_git() {
        assert_eq!(
            classify_source("git@github.com:foo/bar.git").unwrap(),
            Source::Git {
                url: "git@github.com:foo/bar.git".to_string(),
                git_ref: None
            }
        );
        assert_eq!(
            classify_source("git@github.com:foo/bar.git#dev").unwrap(),
            Source::Git {
                url: "git@github.com:foo/bar.git".to_string(),
                git_ref: Some("dev".to_string())
            }
        );
    }

    /// MP-5 — a bare GitLab project URL is a git clone, not a hosted
    /// `marketplace.json`. Before this only `is_github_host` got the `.git`
    /// treatment, so every GitLab marketplace fell through to `Source::Url` and
    /// was fetched as JSON.
    #[test]
    fn classify_gitlab_project_urls_as_git() {
        for (input, expected) in [
            (
                "https://gitlab.com/group/project",
                "https://gitlab.com/group/project.git",
            ),
            // Nested subgroups — GitLab's shape, and why `owner/repo` is only a floor.
            (
                "https://gitlab.com/group/subgroup/project",
                "https://gitlab.com/group/subgroup/project.git",
            ),
            // Self-hosted.
            (
                "https://gitlab.acme.com/g/p",
                "https://gitlab.acme.com/g/p.git",
            ),
            // Trailing slash trimmed before `.git` is appended.
            (
                "https://gitlab.com/group/project/",
                "https://gitlab.com/group/project.git",
            ),
        ] {
            assert_eq!(
                classify_source(input).unwrap(),
                Source::Git {
                    url: expected.to_string(),
                    git_ref: None
                },
                "{input}"
            );
        }
        assert_eq!(
            classify_source("https://gitlab.com/group/project#dev").unwrap(),
            Source::Git {
                url: "https://gitlab.com/group/project.git".to_string(),
                git_ref: Some("dev".to_string())
            }
        );
    }

    /// The oracle's guards are load-bearing: each of these names something that
    /// is NOT a project, and must stay a `url` source.
    #[test]
    fn gitlab_urls_that_do_not_name_a_project_stay_url_sources() {
        for input in [
            // `/-/` introduces a sub-resource, so this is a page in a project.
            "https://gitlab.com/group/project/-/merge_requests/5",
            "https://gitlab.com/group/project/-/tree/main",
            // The REST API.
            "https://gitlab.com/api/v4/projects/1",
            // Only one segment.
            "https://gitlab.com/group",
        ] {
            assert!(
                matches!(classify_source(input).unwrap(), Source::Url { .. }),
                "{input} must not be treated as a clonable project"
            );
        }
    }

    #[test]
    fn classify_http_git_urls() {
        // github.com/owner/repo → git, `.git` appended.
        assert_eq!(
            classify_source("https://github.com/anthropics/claude-plugins-official").unwrap(),
            Source::Git {
                url: "https://github.com/anthropics/claude-plugins-official.git".to_string(),
                git_ref: None
            }
        );
        // `www.` is stripped for the host test.
        assert_eq!(
            classify_source("https://www.github.com/o/r").unwrap(),
            Source::Git {
                url: "https://www.github.com/o/r.git".to_string(),
                git_ref: None
            }
        );
        // Any host ending `.git` → git (with `#ref` split off).
        assert_eq!(
            classify_source("https://gitlab.com/foo/bar.git#main").unwrap(),
            Source::Git {
                url: "https://gitlab.com/foo/bar.git".to_string(),
                git_ref: Some("main".to_string())
            }
        );
        // `/_git/` (Azure DevOps) → git.
        assert_eq!(
            classify_source("https://dev.azure.com/org/proj/_git/repo").unwrap(),
            Source::Git {
                url: "https://dev.azure.com/org/proj/_git/repo".to_string(),
                git_ref: None
            }
        );
    }

    #[test]
    fn classify_http_url_sources() {
        // Non-github host, no `.git` → hosted marketplace.json (url).
        assert_eq!(
            classify_source("https://example.com/marketplace.json").unwrap(),
            Source::Url {
                url: "https://example.com/marketplace.json".to_string()
            }
        );
        // github.com WITHOUT an owner/repo path → url, not git.
        assert_eq!(
            classify_source("https://github.com/onlyone").unwrap(),
            Source::Url {
                url: "https://github.com/onlyone".to_string()
            }
        );
    }

    #[test]
    fn classify_directory_and_missing() {
        let e = full_env();
        let abs = std::fs::canonicalize(&e.market).unwrap();
        assert_eq!(
            classify_source(&e.market.to_string_lossy()).unwrap(),
            Source::Directory(abs)
        );
        assert_eq!(
            classify_source("/no/such/dir").unwrap_err(),
            "✘ Path does not exist: /no/such/dir"
        );
    }

    // ---- settings/registry `source` object shaping ----

    #[test]
    fn source_object_shapes() {
        assert_eq!(
            source_object(
                &Source::Github {
                    repo: "a/b".to_string(),
                    git_ref: None
                },
                &[]
            ),
            json!({"source": "github", "repo": "a/b"})
        );
        assert_eq!(
            source_object(
                &Source::Github {
                    repo: "a/b".to_string(),
                    git_ref: Some("v1".to_string())
                },
                &[]
            ),
            json!({"source": "github", "repo": "a/b", "ref": "v1"})
        );
        assert_eq!(
            source_object(
                &Source::Git {
                    url: "https://x/y.git".to_string(),
                    git_ref: None
                },
                &[]
            ),
            json!({"source": "git", "url": "https://x/y.git"})
        );
        assert_eq!(
            source_object(
                &Source::Git {
                    url: "https://x/y.git".to_string(),
                    git_ref: Some("dev".to_string())
                },
                &[]
            ),
            json!({"source": "git", "url": "https://x/y.git", "ref": "dev"})
        );
        assert_eq!(
            source_object(
                &Source::Url {
                    url: "https://x/cat.json".to_string()
                },
                &[]
            ),
            json!({"source": "url", "url": "https://x/cat.json"})
        );
        assert_eq!(
            source_object(&Source::Directory(PathBuf::from("/abs/mkt")), &[]),
            json!({"source": "directory", "path": "/abs/mkt"})
        );
    }

    /// `--sparse` (oracle `sparsePaths`) is recorded on github/git — the two
    /// kinds the guard in `run_add` allows it for — and is a no-op everywhere
    /// else (never reached with a non-empty `sparse` in practice, since the
    /// guard rejects it earlier, but `source_object` itself stays honest).
    #[test]
    fn source_object_records_sparse_paths_for_github_and_git_only() {
        let paths = vec![".claude-plugin".to_string(), "plugins".to_string()];
        assert_eq!(
            source_object(
                &Source::Github {
                    repo: "a/b".to_string(),
                    git_ref: None
                },
                &paths
            ),
            json!({"source": "github", "repo": "a/b", "sparsePaths": [".claude-plugin", "plugins"]})
        );
        assert_eq!(
            source_object(
                &Source::Git {
                    url: "https://x/y.git".to_string(),
                    git_ref: None
                },
                &paths
            ),
            json!({"source": "git", "url": "https://x/y.git", "sparsePaths": [".claude-plugin", "plugins"]})
        );
        // Directory/Url never carry sparsePaths even if (hypothetically) asked.
        assert_eq!(
            source_object(&Source::Directory(PathBuf::from("/abs/mkt")), &paths),
            json!({"source": "directory", "path": "/abs/mkt"})
        );
    }

    /// The oracle: *"--sparse is only supported for github and git
    /// marketplace sources"*. `run_add` must reject it (byte-exact message)
    /// for every other classified source kind, and the registry/settings
    /// declaration must never silently drop it for github/git (the prior
    /// port behavior: `_sparse` was accepted and discarded).
    #[test]
    fn sparse_is_rejected_for_non_github_git_sources_with_the_oracle_message() {
        let e = full_env();
        let err = run_add(
            "https://example.test/marketplace.json",
            None,
            &["plugins".to_string()],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "✘ --sparse is only supported for github and git marketplace sources (got: url)"
        );
    }

    /// Oracle `qn` validates `--scope` BEFORE the `--sparse` kind guard, so
    /// with both wrong the caller sees the scope error. (Both message bodies
    /// are byte-exact already; only the precedence diverged.)
    #[test]
    fn an_invalid_scope_beats_the_sparse_kind_guard() {
        let e = full_env();
        let err = run_add(
            "https://example.test/marketplace.json",
            Some("bogus"),
            &["plugins".to_string()],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "✘ Invalid scope \'bogus\'. Use: user, project, or local"
        );
    }

    /// `prune_to_sparse_paths` — the working-tree narrowing that stands in for
    /// real cone-mode sparse-checkout (see its doc comment: no safe
    /// sparse-checkout binding is available over this crate's vendored-libgit2
    /// transport). Top-level files and `.git` always survive; a top-level
    /// directory not named by any `sparse_paths` entry does not.
    #[test]
    fn prune_to_sparse_paths_keeps_only_named_top_level_dirs_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(root.join(".claude-plugin/marketplace.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("plugins/foo")).unwrap();
        std::fs::create_dir_all(root.join("unrelated-monorepo-package")).unwrap();
        std::fs::write(root.join("README.md"), "hi").unwrap();

        prune_to_sparse_paths(root, &[".claude-plugin".to_string(), "plugins".to_string()])
            .unwrap();

        assert!(root.join(".git").is_dir(), ".git must survive pruning");
        assert!(root.join(".claude-plugin/marketplace.json").is_file());
        assert!(root.join("plugins/foo").is_dir());
        assert!(
            root.join("README.md").is_file(),
            "top-level files always survive"
        );
        assert!(
            !root.join("unrelated-monorepo-package").exists(),
            "an un-listed top-level directory must be pruned"
        );
    }

    /// `--sparse` end to end: `clone_marketplace` (the actual clone helper
    /// `run_add` calls) must narrow the working tree it hands back, not just
    /// record `sparsePaths` in the registry — the audited gap was that a prior
    /// port version discarded `--sparse` entirely, so neither happened.
    #[cfg(unix)]
    #[test]
    fn clone_marketplace_prunes_the_working_tree_to_sparse_paths() {
        let repo_root = tempfile::tempdir().unwrap();
        let repo = repo_root.path().join("repo");
        std::fs::create_dir_all(repo.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        std::fs::write(
            repo.join(branding::PLUGIN_MANIFEST_DIR)
                .join("marketplace.json"),
            r#"{"name":"sparse-mkt","owner":{"name":"me"},"plugins":[]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(repo.join("unrelated-package")).unwrap();
        std::fs::write(repo.join("unrelated-package/file.txt"), "x").unwrap();
        for args in [
            vec!["init", repo.to_str().unwrap()],
            vec!["-C", repo.to_str().unwrap(), "add", "."],
            vec![
                "-C",
                repo.to_str().unwrap(),
                "-c",
                "user.name=LingXi Test",
                "-c",
                "user.email=lingxi@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        ] {
            let output = std::process::Command::new("git")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let e = env();
        let (name, clone_dir) = clone_marketplace(
            &e.plugins,
            &format!("file://{}", repo.display()),
            "sparse-hint",
            None,
            &[branding::PLUGIN_MANIFEST_DIR.to_string()],
        )
        .unwrap();

        assert_eq!(name, "sparse-mkt");
        assert!(clone_dir
            .join(branding::PLUGIN_MANIFEST_DIR)
            .join("marketplace.json")
            .is_file());
        assert!(
            !clone_dir.join("unrelated-package").exists(),
            "a directory outside --sparse must not survive in the clone"
        );
    }

    #[test]
    fn url_repo_hint_strips_git_suffix() {
        assert_eq!(url_repo_hint("https://gitlab.com/foo/bar.git"), "bar");
        assert_eq!(url_repo_hint("git@github.com:foo/baz.git"), "baz");
        assert_eq!(url_repo_hint("https://example.com/deep/repo/"), "repo");
    }

    #[test]
    fn hosted_marketplace_requires_https_before_network_io() {
        let e = full_env();
        let err = fetch_hosted_marketplace(&e.plugins, "http://example.com/marketplace.json")
            .unwrap_err();
        assert_eq!(err, "Hosted marketplace URLs must use HTTPS");
        assert!(!e.plugins.join("marketplaces").exists());
    }

    #[test]
    fn add_invalid_source_errors_without_prefix() {
        let e = full_env();
        // Classification error precedes even scope validation (like directory
        // path-not-exist) — no "Adding marketplace…" prefix.
        let err = run_add(
            "not a repo",
            Some("bogus"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "✘ Invalid marketplace source format. Try: owner/repo, https://..., or ./path"
        );
    }

    #[test]
    fn add_remote_writes_registry_and_declaration() {
        // Network-backed end-to-end clone; opt-in (needs a reachable public
        // marketplace repo). Set LINGXI_PLUGIN_NET_TEST=owner/repo to run.
        let Ok(repo) = std::env::var("LINGXI_PLUGIN_NET_TEST") else {
            return;
        };
        let e = full_env();
        let msg = run_add(&repo, None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        assert!(
            msg.starts_with("Adding marketplace…✔ Successfully added marketplace: "),
            "got: {msg}"
        );
        assert!(msg.ends_with("(declared in user settings)"), "got: {msg}");
        // A registry entry with a git/github source + installLocation was written.
        let reg = registry_of(&e);
        let (_, entry) = reg.as_object().unwrap().iter().next().unwrap();
        let kind = entry["source"]["source"].as_str().unwrap();
        assert!(matches!(kind, "git" | "github"), "kind: {kind}");
        assert!(entry["installLocation"]
            .as_str()
            .unwrap()
            .contains("marketplaces"));
    }

    #[test]
    fn update_unknown_lists_available() {
        let e = full_env();
        let src = e.market.to_string_lossy().to_string();
        run_add(&src, None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        let err = run_update(Some("ghost"), &e.plugins, &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "Updating marketplace: ghost...✘ Failed to update marketplace(s): Marketplace 'ghost' not found. Available marketplaces: mymkt"
        );
    }
}
