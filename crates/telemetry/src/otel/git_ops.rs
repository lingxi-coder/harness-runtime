//! Bash-command git/PR detection for the `claude_code.commit.count` and
//! `claude_code.pull_request.count` counters.
//!
//! 1:1 port of the COUNTER subset of the binary's `mEo(command, code, output)`
//! (2.1.220): on a zero exit code,
//!
//! - `fur("commit")` (`\bgit(?:\s+-[cC]\s+\S+|\s+--\S+=\S+)*\s+commit\b`)
//!   ⇒ `jSi()?.add(1)` — one commit-count increment (also for `--amend`;
//!   CC emits an extra `tengu_git_operation commit_amend` product event but
//!   still adds exactly once);
//! - the first matching `mrd` entry with `action:"created"` — i.e.
//!   `\bgh\s+pr\s+create\b`, which is first in the array — ⇒ `lAt()?.add(1)`;
//! - `\bglab\s+mr\s+create\b` ⇒ `lAt()?.add(1)`;
//! - `curl` POST against a PR/MR API URL (`\bcurl\b` + one of `-X POST`,
//!   `--request[=]POST`, a `-d` body flag, + an
//!   `https?://…/(pulls|pull-requests|merge[-_]requests)` URL not followed by
//!   `/<digit>`) ⇒ `lAt()?.add(1)`.
//!
//! The three PR arms are independent `if`s in CC, so a command hitting several
//! increments several times — preserved here. Neither counter carries
//! attributes (`add(1)` bare in the binary).
//!
//! The matchers are hand-rolled ports of the JS regexes (the `telemetry` crate
//! carries no `regex` dependency, and the `(?!\/\d)` lookahead would need
//! manual handling anyway). JS `\s`/`\S` are approximated by
//! `char::is_whitespace` (+ U+FEFF, which JS counts as whitespace but Rust does
//! not); `\b`/`\w` use the exact JS `[A-Za-z0-9_]` word class.
//!
//! The sibling `tengu_git_operation` PRODUCT telemetry events (`M(...)` in
//! `mEo`) and the PR session-linking chain (`Ltn`/`Udt`) remain unported —
//! this module is scoped to the two OTEL instruments.

/// Counter increments detected in one completed bash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct GitCounterHits {
    /// `claude_code.commit.count` increments (0 or 1).
    pub commits: u32,
    /// `claude_code.pull_request.count` increments (0–3; independent arms).
    pub pull_requests: u32,
}

/// Scan `command` for the counter-relevant git/PR operations. The caller is
/// responsible for the `exit_code == 0` gate (CC: `if(t!==0)return`).
pub(crate) fn git_counter_hits(command: &str) -> GitCounterHits {
    let mut hits = GitCounterHits::default();
    if matches_git_subcommand(command, "commit") {
        hits.commits += 1;
    }
    // mrd.find(...) — `gh pr create` is the FIRST array entry, so "the found
    // entry has action created" reduces to "the create pattern matches".
    if matches_word_seq(command, &["gh", "pr", "create"]) {
        hits.pull_requests += 1;
    }
    if matches_word_seq(command, &["glab", "mr", "create"]) {
        hits.pull_requests += 1;
    }
    if is_curl_pr_api_post(command) {
        hits.pull_requests += 1;
    }
    hits
}

/// JS `\w` — exactly `[A-Za-z0-9_]` (no unicode classes in the CC patterns).
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// JS `\b` before byte index `idx`.
fn boundary_before(s: &[u8], idx: usize) -> bool {
    idx == 0 || !is_word_byte(s[idx - 1])
}

/// JS `\b` after byte index `end` (exclusive).
fn boundary_after(s: &[u8], end: usize) -> bool {
    end >= s.len() || !is_word_byte(s[end])
}

/// JS `\s` (whitespace incl. the BOM, which `char::is_whitespace` excludes).
fn is_js_ws(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// Byte index after the run of JS whitespace starting at `pos`.
fn skip_ws(s: &str, pos: usize) -> usize {
    s[pos..]
        .char_indices()
        .find(|(_, c)| !is_js_ws(*c))
        .map_or(s.len(), |(i, _)| pos + i)
}

/// Byte index after the run of JS non-whitespace (`\S`) starting at `pos`.
fn skip_non_ws(s: &str, pos: usize) -> usize {
    s[pos..]
        .char_indices()
        .find(|(_, c)| is_js_ws(*c))
        .map_or(s.len(), |(i, _)| pos + i)
}

/// `fur(sub)` — `\bgit(?:\s+-[cC]\s+\S+|\s+--\S+=\S+)*\s+${sub}\b`.
fn matches_git_subcommand(command: &str, sub: &str) -> bool {
    let bytes = command.as_bytes();
    let mut search = 0;
    while let Some(rel) = command[search..].find("git") {
        let start = search + rel;
        search = start + 1;
        if !boundary_before(bytes, start) {
            continue;
        }
        // Consume `(?:\s+-[cC]\s+\S+|\s+--\S+=\S+)*` greedily. On a partial
        // group match the loop stops and the `\s+${sub}` tail is tried from
        // the last complete-iteration position (the JS backtrack outcome for
        // every command shape the alternation can produce).
        let mut pos = start + 3;
        loop {
            let flag_start = skip_ws(command, pos);
            if flag_start == pos {
                break; // no `\s+`
            }
            let rest = &command[flag_start..];
            if rest.starts_with("-c") || rest.starts_with("-C") {
                // `-[cC]\s+\S+` — the value is REQUIRED to be space-separated.
                let value_start = skip_ws(command, flag_start + 2);
                if value_start == flag_start + 2 {
                    break;
                }
                let value_end = skip_non_ws(command, value_start);
                if value_end == value_start {
                    break;
                }
                pos = value_end;
            } else if let Some(tok) = rest.strip_prefix("--") {
                // `--\S+=\S+` — within the token: `--`, ≥1 `\S`, `=`, ≥1 `\S`.
                let tok_end = skip_non_ws(command, flag_start);
                let tok_len = tok_end - flag_start - 2;
                match tok[..tok_len].find('=') {
                    Some(eq) if eq > 0 && eq + 1 < tok_len => pos = tok_end,
                    _ => break,
                }
            } else {
                break;
            }
        }
        // `\s+${sub}\b`
        let sub_start = skip_ws(command, pos);
        if sub_start == pos {
            continue;
        }
        if command[sub_start..].starts_with(sub) && boundary_after(bytes, sub_start + sub.len()) {
            return true;
        }
    }
    false
}

/// `\b${w0}\s+${w1}\s+…\s+${wN}\b` (the `mrd` / glab patterns).
fn matches_word_seq(command: &str, words: &[&str]) -> bool {
    let bytes = command.as_bytes();
    let first = words[0];
    let mut search = 0;
    while let Some(rel) = command[search..].find(first) {
        let start = search + rel;
        search = start + 1;
        if !boundary_before(bytes, start) {
            continue;
        }
        let mut pos = start + first.len();
        let mut ok = true;
        for word in &words[1..] {
            let word_start = skip_ws(command, pos);
            if word_start == pos || !command[word_start..].starts_with(word) {
                ok = false;
                break;
            }
            pos = word_start + word.len();
        }
        if ok && boundary_after(bytes, pos) {
            return true;
        }
    }
    false
}

/// The curl arm: `\bcurl\b` AND (`/-X\s*POST\b/i` OR `/--request\s*=?\s*POST\b/i`
/// OR `/\s-d\s/`) AND the PR/MR API URL predicate.
fn is_curl_pr_api_post(command: &str) -> bool {
    if !matches_word_seq(command, &["curl"]) {
        return false;
    }
    let lower = command.to_lowercase();
    let post_flag = find_ci_then_post(&lower, "-x", false)
        || find_ci_then_post(&lower, "--request", true)
        || has_space_d_space(command);
    post_flag && has_pr_api_url(&lower)
}

/// `/-X\s*POST\b/i` (`allow_eq=false`) / `/--request\s*=?\s*POST\b/i`
/// (`allow_eq=true`), evaluated over the pre-lowercased command.
fn find_ci_then_post(lower: &str, flag: &str, allow_eq: bool) -> bool {
    let bytes = lower.as_bytes();
    let mut search = 0;
    while let Some(rel) = lower[search..].find(flag) {
        let start = search + rel;
        search = start + 1;
        let mut pos = skip_ws(lower, start + flag.len());
        if allow_eq && bytes.get(pos) == Some(&b'=') {
            pos = skip_ws(lower, pos + 1);
        }
        if lower[pos..].starts_with("post") && boundary_after(bytes, pos + 4) {
            return true;
        }
    }
    false
}

/// `/\s-d\s/` — a whitespace-delimited `-d` flag (case-sensitive).
fn has_space_d_space(command: &str) -> bool {
    let mut chars: Vec<char> = Vec::with_capacity(4);
    for c in command.chars() {
        chars.push(c);
        let n = chars.len();
        if n >= 4
            && is_js_ws(chars[n - 4])
            && chars[n - 3] == '-'
            && chars[n - 2] == 'd'
            && is_js_ws(chars[n - 1])
        {
            return true;
        }
    }
    false
}

/// `/https?:\/\/[^\s'"]*\/(pulls|pull-requests|merge[-_]requests)(?!\/\d)/i`
/// over the pre-lowercased command. The JS engine backtracks over every
/// candidate segment position, so the predicate is: SOME occurrence of a PR/MR
/// path segment that (a) is not followed by `/<digit>` and (b) is preceded —
/// within an unbroken `[^\s'"]` run — by `https?://`.
fn has_pr_api_url(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    for segment in [
        "/pulls",
        "/pull-requests",
        "/merge_requests",
        "/merge-requests",
    ] {
        let mut search = 0;
        while let Some(rel) = lower[search..].find(segment) {
            let seg_start = search + rel;
            search = seg_start + 1;
            let seg_end = seg_start + segment.len();
            // `(?!\/\d)`
            if bytes.get(seg_end) == Some(&b'/')
                && bytes.get(seg_end + 1).is_some_and(u8::is_ascii_digit)
            {
                continue;
            }
            if preceded_by_url_scheme(lower, seg_start) {
                return true;
            }
        }
    }
    false
}

/// Whether some `https?://` begins at or before `pos` with only `[^\s'"]`
/// characters between the scheme and `pos`.
fn preceded_by_url_scheme(lower: &str, pos: usize) -> bool {
    let mut search = 0;
    while let Some(rel) = lower[search..pos].find("http") {
        let start = search + rel;
        search = start + 1;
        let after = &lower[start..];
        let scheme_len = if after.starts_with("https://") {
            8
        } else if after.starts_with("http://") {
            7
        } else {
            continue;
        };
        if start + scheme_len > pos {
            continue;
        }
        let clean = lower[start + scheme_len..pos]
            .chars()
            .all(|c| !is_js_ws(c) && c != '\'' && c != '"');
        if clean {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(cmd: &str) -> (u32, u32) {
        let h = git_counter_hits(cmd);
        (h.commits, h.pull_requests)
    }

    #[test]
    fn plain_git_commit_counts_once() {
        assert_eq!(hits("git commit -m 'msg'"), (1, 0));
        assert_eq!(hits("cd repo && git commit"), (1, 0));
        // `--amend` still adds exactly once.
        assert_eq!(hits("git commit --amend --no-edit"), (1, 0));
    }

    #[test]
    fn git_commit_with_config_flags_matches() {
        // `(?:\s+-[cC]\s+\S+|\s+--\S+=\S+)*` — the fur() flag bridge.
        assert_eq!(hits("git -c user.name=x commit -m hi"), (1, 0));
        assert_eq!(hits("git -C /repo commit"), (1, 0));
        assert_eq!(hits("git --git-dir=/g --work-tree=/w commit"), (1, 0));
        assert_eq!(hits("git -c a=b --git-dir=/g commit"), (1, 0));
    }

    #[test]
    fn non_commit_git_commands_do_not_count() {
        assert_eq!(hits("git status"), (0, 0));
        assert_eq!(hits("git push"), (0, 0));
        assert_eq!(hits("git log --oneline"), (0, 0));
        assert_eq!(hits("git log commit"), (0, 0)); // `log` breaks the bridge
        assert_eq!(hits("mygit commit"), (0, 0)); // no \b before git
        assert_eq!(hits("gitcommit"), (0, 0)); // ditto, run together
        assert_eq!(hits("git commits"), (0, 0)); // no \b after commit
    }

    /// `fur("commit")` is an unanchored regex, so it matches wherever the
    /// sequence appears — after a `cd`, after a `;`, or with the whole command
    /// padded. It also matches inside an `echo`, which looks like a false
    /// positive but is exactly what the oracle counts; the port must not be
    /// "smarter" than the thing it reproduces.
    #[test]
    fn the_commit_pattern_is_unanchored_like_the_oracle() {
        assert_eq!(hits("  git commit  "), (1, 0));
        assert_eq!(hits("cd /tmp && git commit -m x"), (1, 0));
        assert_eq!(hits("git add -A; git commit -m x"), (1, 0));
        // Deliberate: CC's `prd`/`fur` would match this too.
        assert_eq!(hits("echo git commit"), (1, 0));
    }

    /// The `gh pr` verbs that are `mrd` entries WITHOUT `action:"created"`, plus
    /// `gh issue create`, which is a different noun entirely.
    #[test]
    fn non_creating_gh_verbs_do_not_count() {
        for cmd in [
            "gh pr view 1",
            "gh pr list",
            "gh issue create",
            "gh pr merge",
        ] {
            assert_eq!(hits(cmd), (0, 0), "{cmd} must not count as a PR create");
        }
    }

    #[test]
    fn gh_pr_create_counts_a_pull_request() {
        assert_eq!(hits("gh pr create --fill"), (0, 1));
        assert_eq!(hits("git push && gh pr create -t x"), (0, 1));
        // Other gh pr verbs are `mrd` entries without `action:"created"`.
        assert_eq!(hits("gh pr merge 12"), (0, 0));
        assert_eq!(hits("gh pr edit 12"), (0, 0));
    }

    #[test]
    fn glab_mr_create_counts_a_pull_request() {
        assert_eq!(hits("glab mr create --fill"), (0, 1));
    }

    #[test]
    fn curl_post_to_pr_api_counts_a_pull_request() {
        assert_eq!(
            hits("curl -X POST https://api.github.com/repos/o/r/pulls"),
            (0, 1)
        );
        assert_eq!(
            hits("curl --request POST https://gitlab.com/api/v4/projects/1/merge_requests"),
            (0, 1)
        );
        assert_eq!(
            hits("curl -d '{}' https://example.com/x/pull-requests"),
            (0, 1)
        );
        // `(?!\/\d)` — an existing-PR URL is NOT a creation.
        assert_eq!(
            hits("curl -X POST https://api.github.com/repos/o/r/pulls/17"),
            (0, 0)
        );
        // GET against the API is not a creation either.
        assert_eq!(hits("curl https://api.github.com/repos/o/r/pulls"), (0, 0));
        // POST without any PR URL.
        assert_eq!(hits("curl -X POST https://example.com/api"), (0, 0));
    }

    #[test]
    fn independent_arms_can_stack() {
        // glab + curl arms are independent ifs in `mEo` — both fire.
        assert_eq!(
            hits("glab mr create || curl -X POST https://gl/x/merge_requests"),
            (0, 2)
        );
        // commit + pr in one compound command.
        assert_eq!(hits("git commit -m x && gh pr create"), (1, 1));
    }
}
