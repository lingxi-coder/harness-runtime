//! Git plugin-source clone (Stage 1 install arm).
//!
//! ## MP-6 `skipLfs`: already the permanent behaviour here, and NOT testable cheaply
//!
//! Upstream's `skipLfs` sets `GIT_LFS_SKIP_SMUDGE=1` on clone and pull so LFS
//! pointer files stay as pointers instead of downloading their content. That
//! variable is read by the git-lfs SMUDGE FILTER, which git runs through a
//! `filter.<driver>.smudge` config entry. This clone is libgit2, which does not
//! execute external filter drivers at all — so a pointer is never smudged here
//! and `skipLfs: true` is already what happens, unconditionally.
//!
//! ⚠️ The DIVERGENCE is the other direction: `skipLfs: false` — upstream's
//! default — cannot be honoured, because this port never downloads LFS content
//! for a plugin or marketplace clone. Failure direction is "pointer files on
//! disk, less bandwidth", never "surprise multi-GB fetch", and a marketplace's
//! `marketplace.json` is not an LFS object.
//!
//! ⛔ Do not "prove" this with a fixture repo that configures
//! `filter.x.smudge` and asserts the clone produced the raw blob. I wrote that
//! test; it passed; its CONTROL — the same clone through the git CLI, which
//! must produce the SMUDGED content or the fixture proves nothing — failed.
//! Filter drivers are read from the CLONE's config, not the origin's, so a
//! repository cannot ship one and neither git nor libgit2 smudges anything.
//! (git-lfs works because `git lfs install` writes `filter.lfs.*` into the
//! user's GLOBAL config.) A live fixture therefore needs process-global config
//! surgery — `GIT_CONFIG_GLOBAL`, or `git2::opts::set_search_path` — which is
//! exactly the process-global state that makes a parallel suite flake. The
//! claim above is reasoned from libgit2 having no external-driver support, and
//! it is labelled as reasoned rather than dressed up as measured.
//!
//! ## MP-6 `--sparse`: done, elsewhere
//!
//! `sparsePaths` IS consumed — `configuration_admin::plugin_marketplace`'s
//! `prune_to_sparse_paths` drops every non-listed top-level entry right after
//! the clone, on the only path that materialises one. libgit2 has no
//! sparse-checkout, so the port clones fully and prunes: same end state on
//! disk, more bandwidth than upstream's `sparse-checkout set --cone`. An audit
//! that greps only this file concludes the opposite.
//!
//! Unlike the Android `tool-git-mobile` clone (which is anchored to the sandbox
//! workspace root), the plugin cache lives under `~/.lingxi/plugins`, OUTSIDE any
//! workspace, so this clone is intentionally NOT workspace-anchored. It is a thin
//! synchronous wrapper over the same vendored libgit2; callers run it on a
//! blocking thread (`spawn_blocking`).

use std::path::Path;

/// Shallow-clone `url` at `ref_` (a branch/tag; empty = the remote's default
/// branch) into `dest`, which must NOT already exist. Returns the resolved HEAD
/// commit id on success.
///
/// # Errors
/// Returns a human-readable message on an unsupported URL protocol or any clone
/// failure (the string is surfaced verbatim as the install error detail).
pub fn clone_plugin_git(url: &str, ref_: &str, dest: &Path) -> Result<String, String> {
    clone_plugin_git_pinned(url, ref_, None, dest)
}

/// `clone_plugin_git` with an optional `sha` pin.
///
/// Oracle `ohr`: when a `sha` is present the clone runs with `--no-checkout`
/// and WITHOUT `--branch`, then `git fetch --depth 1 origin <sha>` (falling
/// back to `--unshallow`), then `git checkout <sha>` — only after which the
/// `rev-parse HEAD` verification runs. A pin therefore names a commit to
/// CHECK OUT, not merely a tip to assert against.
///
/// This port takes the oracle's `--unshallow` fallback shape directly: with a
/// pin the clone is not depth-limited (so every commit reachable from any
/// branch is present) and the pinned commit is then checked out detached.
/// More bandwidth than the shallow fast path, never less history.
///
/// # Errors
/// Returns a human-readable message on an unsupported URL protocol, a `sha`
/// or `ref_` beginning with `-`, a clone failure, or a pinned commit that is
/// missing from the fetched history.
pub fn clone_plugin_git_pinned(
    url: &str,
    ref_: &str,
    sha: Option<&str>,
    dest: &Path,
) -> Result<String, String> {
    // Oracle `ohr`'s first two lines: neither value may be mistaken for a
    // git option.
    if let Some(sha) = sha.filter(|sha| sha.starts_with('-')) {
        return Err(format!("Invalid sha \"{sha}\": cannot start with \"-\""));
    }
    if ref_.starts_with('-') {
        return Err(format!("Invalid ref \"{ref_}\": cannot start with \"-\""));
    }
    // URL protocol guard: HTTPS / HTTP / file:// and SSH (`git@…`) only.
    let supported = url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("file://")
        || url.starts_with("git@")
        || url.starts_with("ssh://");
    if !supported {
        return Err(format!(
            "Invalid git URL protocol: {url}. Only HTTPS, HTTP, file:// and SSH (git@) URLs are supported."
        ));
    }

    let mut fetch_opts = git2::FetchOptions::new();
    // Shallow (`--depth 1`) for remotes; the libgit2 LOCAL transport rejects a
    // shallow fetch, and git itself does a full clone for `file://` anyway, so
    // skip depth for local sources. A `sha` pin also needs the full history —
    // the pinned commit is almost never the tip.
    if !url.starts_with("file://") && sha.is_none() {
        fetch_opts.depth(1);
    }

    let mut builder = git2::build::RepoBuilder::new();
    builder.fetch_options(fetch_opts);
    // Oracle: `--branch <ref>` is passed only when there is no `sha`.
    if !ref_.is_empty() && sha.is_none() {
        builder.branch(ref_);
    }

    let repo = builder
        .clone(url, dest)
        .map_err(|e| format!("Failed to clone repository: {}", e.message()))?;
    if let Some(sha) = sha {
        checkout_pinned_commit(&repo, sha)?;
    }
    // Recurse submodules (the binary clones with `--recurse-submodules`); a
    // plugin whose components live in a submodule would otherwise land an
    // incomplete tree. (`--shallow-submodules` is not expressible via this git2
    // API, so submodules fetch at full depth — more data, never less.)
    update_submodules_recursive(&repo)
        .map_err(|e| format!("Failed to clone repository: {}", e.message()))?;
    let head = repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .map_err(|e| e.message().to_string())?;
    Ok(head.id().to_string())
}

/// Detach HEAD at `sha` and materialize its tree (oracle: `git checkout <sha>`
/// after the pinned fetch).
fn checkout_pinned_commit(repo: &git2::Repository, sha: &str) -> Result<(), String> {
    // The oracle reaches `git checkout <sha>` for a sha that is simply not in
    // the repository (its `--unshallow` fallback fetch succeeds; the checkout
    // is what fails), so an unresolvable revspec wears the checkout message.
    let object = repo
        .revparse_single(sha)
        .map_err(|e| format!("Failed to checkout commit {sha}: {}", e.message()))?;
    let commit = object
        .peel_to_commit()
        .map_err(|e| format!("Failed to checkout commit {sha}: {}", e.message()))?;
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.force();
    repo.checkout_tree(commit.as_object(), Some(&mut checkout))
        .map_err(|e| format!("Failed to checkout commit {sha}: {}", e.message()))?;
    repo.set_head_detached(commit.id())
        .map_err(|e| format!("Failed to checkout commit {sha}: {}", e.message()))?;
    Ok(())
}

/// Initialise + update every submodule, recursing into nested submodules
/// (the binary's `git submodule update --init --recursive`).
fn update_submodules_recursive(repo: &git2::Repository) -> Result<(), git2::Error> {
    for mut submodule in repo.submodules()? {
        submodule.update(true, None)?; // init = true
        let sub_repo = submodule.open()?;
        update_submodules_recursive(&sub_repo)?;
    }
    Ok(())
}

/// Oracle `pfe` (2.1.251): does this locator's RAW authority carry a
/// backslash, i.e. could two parsers disagree about which host it names?
///
/// Leading `[\x00-\x20]` bytes are stripped first (the oracle's
/// `t.replace(/^[\x00-\x20]+/,"")`), so `" https://evil\\@good/"` is judged
/// on its real scheme. For a "special" scheme (`http`, `https`, `ws`, `wss`,
/// `ftp`) the leading run of `/` and `\` after `://` is inspected and then
/// SKIPPED — those parsers fold `\` into `/` there. The final authority
/// backslash test then runs for EVERY scheme, special or not; the special
/// set only decides whether the leading slash run is looked at separately.
#[must_use]
pub fn is_confusable_authority_url(candidate: &str) -> bool {
    let trimmed = candidate.trim_start_matches(|c: char| (c as u32) <= 0x20);
    let Some(scheme_end) = trimmed.find("://") else {
        return false;
    };
    let mut rest = &trimmed[scheme_end + 3..];
    if matches!(
        trimmed[..scheme_end].to_ascii_lowercase().as_str(),
        "http" | "https" | "ws" | "wss" | "ftp"
    ) {
        let run = rest.len() - rest.trim_start_matches(['/', '\\']).len();
        if rest[..run].contains('\\') {
            return true;
        }
        rest = &rest[run..];
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    rest[..authority_end].contains('\\')
}

/// Oracle `o`: `/[%\x00-\x1f\x7f-\u{10FFFF}]/u` — bytes a hostname has no
/// legitimate reason to carry.
fn host_has_forbidden_chars(host: &str) -> bool {
    host.chars()
        .any(|c| c == '%' || (c as u32) <= 0x1f || (c as u32) >= 0x7f)
}

/// Oracle `^(?:[^@]+@)?([^:]+):` applied to an scp-shorthand locator: the
/// hostname segment, if the string has that shape at all. `[^@]+` cannot
/// cross an `@`, so the optional user part can only ever be the text before
/// the FIRST `@`; the greedy group is tried first and backtracks to absent.
fn scp_hostname(candidate: &str) -> Option<&str> {
    let with_user = match candidate.find('@') {
        Some(at) if at >= 1 => Some(&candidate[at + 1..]),
        _ => None,
    };
    for rest in with_user.into_iter().chain(std::iter::once(candidate)) {
        if let Some(colon) = rest.find(':') {
            if colon > 0 {
                return Some(&rest[..colon]);
            }
        }
    }
    None
}

/// Mirrors the oracle's `ffe` confusable-URL guard (2.1.251), applied at the
/// official-name check, URL-normalisation credential stripping, the policy
/// allowlist matcher, and marketplace classification. A git-ish locator is
/// "suspicious" when it is shaped so two different parsers could disagree
/// about which host it actually names:
///
/// - anything `is_confusable_authority_url` (`pfe`) flags — a backslash in
///   the raw authority of ANY `scheme://` URL;
/// - a `scheme://` URL outside `http`/`https` whose parsed hostname carries
///   `%`, a control character, DEL, or anything above ASCII;
/// - a `scheme://` URL that does not parse at all (the oracle's
///   `catch{return!0}`: unparseable is suspicious, not innocent);
/// - the scp shorthand `[user@]host:path` (no `scheme://` at all) where a
///   `:` appears before the first `@`, or whose hostname segment carries one
///   of those same forbidden characters.
#[must_use]
pub fn is_suspicious_url(candidate: &str) -> bool {
    if candidate.contains("://") {
        if is_confusable_authority_url(candidate) {
            return true;
        }
        return match url::Url::parse(candidate) {
            Ok(parsed) => {
                !matches!(parsed.scheme(), "http" | "https")
                    && host_has_forbidden_chars(parsed.host_str().unwrap_or(""))
            }
            Err(_) => true,
        };
    }
    if let (Some(colon), Some(at)) = (candidate.find(':'), candidate.find('@')) {
        if at > colon {
            return true;
        }
    }
    scp_hostname(candidate).is_some_and(host_has_forbidden_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsupported_protocol() {
        let err = clone_plugin_git("ftp://evil/x", "", Path::new("/tmp/nope")).unwrap_err();
        assert!(err.contains("Invalid git URL protocol"), "{err}");
        assert!(err.contains("ftp://evil/x"), "{err}");
    }

    #[test]
    fn flags_backslash_in_http_authority() {
        // The oracle's own confusable example: some URL parsers treat `\`
        // as `/` inside an http(s) authority, so this can resolve to either
        // `evil.com` or `good.com` depending on which parser reads it.
        assert!(is_suspicious_url(r"https://evil.com\@good.com/"));
        assert!(is_suspicious_url(r"http://evil.com\@good.com/"));
    }

    #[test]
    fn ordinary_urls_are_not_suspicious() {
        assert!(!is_suspicious_url("https://github.com/owner/repo.git"));
        assert!(!is_suspicious_url(
            "git+ssh://git@github.com/owner/repo.git"
        ));
        assert!(!is_suspicious_url("git@github.com:owner/repo.git"));
        assert!(!is_suspicious_url("ssh://git@github.com:22/owner/repo.git"));
    }

    #[test]
    fn flags_control_characters_in_non_http_hostname() {
        // oracle: /[%\x00-\x1f\x7f-\u{10FFFF}]/u applied to the hostname of
        // any scheme outside http/https/ws/wss/ftp.
        assert!(is_suspicious_url("ssh://evil%00host/owner/repo.git"));
        assert!(is_suspicious_url(
            "git+ssh://user@evil\u{0007}host/repo.git"
        ));
    }

    #[test]
    fn flags_backslash_in_a_non_special_scheme_authority() {
        // Oracle `pfe`'s FINAL test — `(r===-1?n:n.slice(0,r)).includes("\\")`
        // — runs for every scheme; the `http/https/ws/wss/ftp` set only gates
        // the separate leading-slash-run inspection above it.
        assert!(is_suspicious_url(
            r"git://evil.example.com\@good.example.com/repo.git"
        ));
        assert!(is_suspicious_url(
            r"git+ssh://evil.example.com\@good.example.com/repo.git"
        ));
        assert!(is_suspicious_url(
            r"ssh://evil.com\@github.mycompany.com:22/x.git"
        ));
    }

    #[test]
    fn strips_leading_control_bytes_before_deciding_the_scheme() {
        // Oracle `pfe` opens with `t=t.replace(/^[\x00-\x20]+/,"")`; without
        // it the scheme reads as `"  https"`, misses the special set, and the
        // confusable authority slips through.
        assert!(is_suspicious_url(
            "  https://evil.example.com\\@good.example.com/"
        ));
        assert!(is_suspicious_url(
            "\u{1}https://evil.example.com\\@good.example.com/"
        ));
    }

    #[test]
    fn flags_forbidden_characters_in_an_scp_form_hostname() {
        // Oracle `ffe` tail: `t.match(/^(?:[^@]+@)?([^:]+):/)?.[1]` then `o(s)`.
        assert!(is_suspicious_url("git@evil%00host:owner/repo.git"));
        assert!(is_suspicious_url("evil\u{7f}host:owner/repo.git"));
    }

    #[test]
    fn an_unparseable_scheme_url_fails_closed() {
        // Oracle `ffe`'s `catch{return!0}`.
        assert!(is_suspicious_url("ssh://[not-an-address/repo.git"));
        assert!(is_suspicious_url("http://"));
    }

    #[test]
    fn flags_colon_before_at_in_scp_form() {
        // A real scp locator never has a colon inside the user segment
        // before the `@`; its presence is the confusion signal itself.
        assert!(is_suspicious_url("evil:trick@host:path"));
        assert!(!is_suspicious_url("git@github.com:owner/repo.git"));
        assert!(!is_suspicious_url("host:path"));
    }
}
