//! Network-intent advisory (spec r3 §Shell tool, D10): deny-net mobile shells
//! refuse commands whose intent is networking up-front with a pointer to the
//! structured Git tool rather than executing to a confusing seccomp EPERM.
//!
//! REACHABILITY — read before extending this module. There are exactly TWO
//! production callers, and which shell you are on decides which one runs:
//!
//!  - `ShellMobileTool::call`, behind `if !mobile_linux_guest` — the legacy
//!    Android shell, where a detected command is REFUSED before it runs.
//!  - `ShellMobileTool::check_permissions`, behind `if guest_shell` — the
//!    iSH/Alpine guest, where it returns `PermissionResult::Ask` so the host
//!    prompts for once/session/always. The guest is not refused, because it is
//!    the only shell that has `apk`, `npm`, `npx` and `pip` at all.
//!
//! `mobile_linux_guest` / `guest_shell` are the same predicate:
//! `ShellCarrier::mobile_linux_guest` sets `force_platform_sandbox: true`.
//!
//! This matters because for a while only the first caller existed, so every
//! rule here for apk/npm/npx/pip was unreachable on the one shell that has
//! them. Adding a rule is not the same as it running — check both call sites.
//!
//! The tokenizer is best-effort (no real shell parse): commands that hide
//! networking via `$(echo cur)l`, `eval`, or similar tricks won't be flagged.
//! For legacy shells the runner's net-deny seccomp filter is the real
//! boundary; for guest shells the existing shell permission gate is the actual
//! control point.

/// Shell command heads that always imply network use.
///
/// Includes the classic external clients (curl/ssh/…) AND the network-capable
/// applets in the bundled toybox inventory (advertised to the model as
/// available), so an egress attempt gets this clean advisory instead of a
/// confusing mid-run seccomp EPERM. Local-only network *introspection* applets
/// (ifconfig/netstat) are deliberately excluded — they query the local
/// interface and are not egress.
const NET_HEADS: &[&str] = &[
    // External clients.
    "curl",
    "wget",
    "nc",
    "ncat",
    "ssh",
    "scp",
    "sftp",
    "rsync",
    "telnet",
    "ftp",
    // Bundled toybox networking applets.
    "netcat",
    "ping",
    "ping6",
    "ftpget",
    "ftpput",
    "httpd",
    "host",
    "sntp",
    "nbd_client",
    "nbd_server",
];

/// `git` subcommands that always hit the network.
const GIT_NET_SUBCMDS: &[&str] = &["clone", "fetch", "pull", "push", "ls-remote"];

/// `git` subcommands that hit the network ONLY in their `… update` form
/// (`git remote update`, `git submodule update`). Their other forms
/// (`git remote -v`, `git remote add`, `git submodule status`) are local and
/// must NOT be refused.
const GIT_NET_SUBCMDS_UPDATE_ONLY: &[&str] = &["remote", "submodule"];

/// `apk` subcommands that ordinarily reach the network unless `--no-network`
/// is present.
const APK_NET_SUBCMDS: &[&str] = &["add", "update", "upgrade", "search", "policy", "fetch"];

/// `npm` subcommands that ordinarily resolve against the registry unless an
/// offline/no-network option is present.
const NPM_NET_SUBCMDS: &[&str] = &[
    // `npm create`/`npm init` resolve a starter package from the registry;
    // the official Vite scaffold therefore follows the same approval path as
    // dependency installation instead of silently bypassing the network gate.
    "create",
    "init",
    "install",
    // npm accepts a family of aliases for `install`; `i` is the canonical
    // shorthand and by far the most common form an agent actually emits, so
    // listing only the long spelling left the usual case undetected.
    "i",
    "in",
    "ins",
    "inst",
    "insta",
    "instal",
    "isnt",
    "isnta",
    "isntal",
    "add",
    "it",
    "install-test",
    "update",
    "up",
    "upgrade",
    "udpate",
    // `npm exec` / `npx` fetch the package they run when it is not installed.
    "exec",
    "dlx",
    "dedupe",
    "ddp",
    "link",
    "pack",
    "ping",
    "star",
    "unstar",
    "deprecate",
    "access",
    "token",
    "profile",
    "hook",
    "org",
    "ci",
    "adduser",
    "audit",
    "dist-tag",
    "doctor",
    "login",
    "logout",
    "outdated",
    "owner",
    "publish",
    "repo",
    "search",
    "team",
    "unpublish",
    "view",
    "whoami",
];

/// `pip` subcommands that ordinarily fetch indexes/artifacts unless
/// `--no-index` is present.
const PIP_NET_SUBCMDS: &[&str] = &["install", "download", "wheel", "index", "search"];

const GIT_SUBCMD_VALUE_OPTS: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--config-env",
    "--super-prefix",
];
const APK_SUBCMD_VALUE_OPTS: &[&str] = &["--root", "--repositories-file", "--keys-dir"];
const NPM_SUBCMD_VALUE_OPTS: &[&str] = &[
    "-C",
    "--prefix",
    "-w",
    "--workspace",
    "--userconfig",
    "--cache",
    "--registry",
];

/// Short options of the transparent wrappers above that take a separate value
/// argument (`sudo -u nobody`, `nice -n 10`, `env -u VAR`, `stdbuf -o0`). Kept
/// as one flat list because a false match here only skips one extra word of a
/// wrapper's own arguments, never the command head itself.
const WRAPPER_VALUE_OPTS: &[&str] = &[
    "-u", "-g", "-U", "-p", "-C", "-h", "-D", "-R", "-T", "-S", "-n", "-i", "-o", "-e", "-f",
];

/// Whether `word` is a leading `NAME=VALUE` shell assignment rather than a
/// command head. Matches the shell's own rule: the name must be a valid
/// identifier, so `--flag=value` and `a.b=c` are not assignments.
fn is_env_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// If the command shows network intent, return a human advisory string
/// (for the tool error). `None` = no detected intent (still run deny-net).
#[must_use]
pub fn network_intent(command: &str) -> Option<String> {
    for seg in split_segments(command) {
        let mut words = seg.split_whitespace();
        // `NODE_ENV=production npm i`, `sudo apk add`, and `env http_proxy=… curl`
        // all put the command that actually runs behind a prefix. Classifying
        // the literal first word let any of them through unflagged, so advance
        // past leading assignments and transparent wrappers first.
        let mut saw_wrapper = false;
        let head = loop {
            let Some(word) = words.next() else {
                break None;
            };
            if is_env_assignment(word) {
                saw_wrapper = true;
                continue;
            }
            let bare = word.rsplit('/').next().unwrap_or(word);
            if matches!(
                bare,
                "sudo" | "doas" | "env" | "nohup" | "command" | "exec" | "time" | "stdbuf" | "nice"
            ) {
                saw_wrapper = true;
                continue;
            }
            // Options belong to the wrapper we just skipped, not to the real
            // head. Short options that take a separate value must also consume
            // it, or `sudo -u nobody curl …` classifies `nobody` and misses the
            // curl entirely. `--opt=value` forms are self-contained.
            if saw_wrapper && word.starts_with('-') {
                if WRAPPER_VALUE_OPTS.contains(&word) {
                    let _ = words.next();
                }
                continue;
            }
            break Some(word);
        };
        let Some(head) = head else {
            continue;
        };
        // Strip any leading non-identifier characters (e.g. `$(`, backticks)
        // and path prefix so `/usr/bin/curl` → `curl`.
        let head = head
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != '/');
        let base = head.rsplit('/').next().unwrap_or(head);
        if NET_HEADS.contains(&base) {
            return Some(format!(
                "`{base}` needs network access. In deny-net mobile shells this is refused \
                 up-front; in guest shells such as iSH/OpenMinis, agent-initiated network \
                 access still requires an approved shell invocation. Use a structured network \
                 tool (e.g. Git) for remote operations when available."
            ));
        }
        if base == "git" {
            let args: Vec<&str> = words.collect();
            if let Some(sub) = next_positional(&args, GIT_SUBCMD_VALUE_OPTS) {
                let is_net = GIT_NET_SUBCMDS.contains(&sub)
                    || (GIT_NET_SUBCMDS_UPDATE_ONLY.contains(&sub)
                        && next_positional_after(&args, sub, GIT_SUBCMD_VALUE_OPTS)
                            == Some("update"));
                if is_net {
                    return Some(format!(
                        "`git {sub}` needs network access. In deny-net mobile shells this is refused \
                         up-front; in guest shells such as iSH/OpenMinis, agent-initiated network \
                         access still requires an approved shell invocation. Use the Git tool for \
                         remote git operations; local git (status/diff/commit/log) works here."
                    ));
                }
            }
            continue;
        }

        if base == "apk" {
            let args: Vec<&str> = words.collect();
            if args.iter().any(|arg| *arg == "--no-network") {
                continue;
            }
            if let Some(sub) = next_positional(&args, APK_SUBCMD_VALUE_OPTS) {
                if APK_NET_SUBCMDS.contains(&sub) {
                    return Some(format!(
                        "`apk {sub}` needs network access. Deny-net mobile shells refuse it up-front; \
                         guest shells such as iSH/OpenMinis require an approved shell invocation for \
                         agent-initiated network access."
                    ));
                }
            }
            continue;
        }

        if base == "npm" {
            let args: Vec<&str> = words.collect();
            if args.iter().any(|arg| *arg == "--offline") {
                continue;
            }
            if let Some(sub) = next_positional(&args, NPM_SUBCMD_VALUE_OPTS) {
                if NPM_NET_SUBCMDS.contains(&sub) {
                    return Some(format!(
                        "`npm {sub}` needs registry access. Deny-net mobile shells refuse it up-front; \
                         guest shells such as iSH/OpenMinis require an approved shell invocation for \
                         agent-initiated network access."
                    ));
                }
            }
            continue;
        }

        if base == "npx" {
            let args: Vec<&str> = words.collect();
            if args.iter().any(|arg| *arg == "--no-install") {
                continue;
            }
            return Some(
                "`npx` may download packages from the registry. Deny-net mobile shells refuse it \
                 up-front; guest shells such as iSH/OpenMinis require an approved shell invocation \
                 for agent-initiated network access."
                    .into(),
            );
        }

        if matches!(base, "pip" | "pip3") {
            let args: Vec<&str> = words.collect();
            if args.iter().any(|arg| *arg == "--no-index") {
                continue;
            }
            if let Some(sub) = next_positional(&args, &[]) {
                if PIP_NET_SUBCMDS.contains(&sub) {
                    return Some(format!(
                        "`{base} {sub}` needs package index access. Deny-net mobile shells refuse \
                         it up-front; guest shells such as iSH/OpenMinis require an approved shell \
                         invocation for agent-initiated network access."
                    ));
                }
            }
            continue;
        }

        if matches!(base, "python" | "python3") {
            let args: Vec<&str> = words.collect();
            if let Some((pip_args, pip_base)) = python_module_args(&args) {
                if pip_args.iter().any(|arg| *arg == "--no-index") {
                    continue;
                }
                if let Some(sub) = next_positional(pip_args, &[]) {
                    if PIP_NET_SUBCMDS.contains(&sub) {
                        return Some(format!(
                            "`{pip_base} {sub}` needs package index access. Deny-net mobile shells \
                             refuse it up-front; guest shells such as iSH/OpenMinis require an \
                             approved shell invocation for agent-initiated network access."
                        ));
                    }
                }
            }
        }
    }
    None
}

fn next_positional<'a>(args: &'a [&'a str], value_opts: &[&str]) -> Option<&'a str> {
    next_positional_impl(args, value_opts, 0).map(|(_, value)| value)
}

fn next_positional_after<'a>(
    args: &'a [&'a str],
    prior: &str,
    value_opts: &[&str],
) -> Option<&'a str> {
    let (idx, _) = next_positional_impl(args, value_opts, 0)?;
    if args[idx] != prior {
        return None;
    }
    next_positional_impl(args, value_opts, idx + 1).map(|(_, value)| value)
}

fn next_positional_impl<'a>(
    args: &'a [&'a str],
    value_opts: &[&str],
    start: usize,
) -> Option<(usize, &'a str)> {
    let mut idx = start;
    while idx < args.len() {
        let arg = args[idx];
        if arg == "--" {
            idx += 1;
            break;
        }
        if value_opts.contains(&arg) {
            idx += 2;
            continue;
        }
        if arg.starts_with("--") && arg.contains('=') {
            idx += 1;
            continue;
        }
        if arg.starts_with('-') {
            idx += 1;
            continue;
        }
        return Some((idx, arg));
    }
    args.get(idx).copied().map(|value| (idx, value))
}

fn python_module_args<'a>(args: &'a [&'a str]) -> Option<(&'a [&'a str], &'a str)> {
    let module_idx = args.iter().position(|arg| *arg == "-m")?;
    let module = args.get(module_idx + 1).copied()?;
    if !matches!(module, "pip" | "pip3") {
        return None;
    }
    Some((&args[(module_idx + 2)..], module))
}

/// Split a command line into top-level segments on `|`, `&&`, `||`, `;`, `&`,
/// and newlines. Best-effort (ignores quoting/`$()` — the runner's seccomp is
/// the real boundary; this is advisory).
///
/// `&&` and `||` collapse to a single boundary (they are two-char operators
/// and must not produce an empty segment between them). Bare `&` (background)
/// and `|` (pipe) each produce a boundary, as do `;` and `\n`.
pub(crate) fn split_segments(command: &str) -> Vec<String> {
    let bytes = command.as_bytes();
    let len = bytes.len();
    let mut segments: Vec<String> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < len {
        let b = bytes[i];
        let (boundary_len, skip_extra) = match b {
            // `&&` → 2-char boundary
            b'&' if i + 1 < len && bytes[i + 1] == b'&' => (2, 0),
            // `||` → 2-char boundary
            b'|' if i + 1 < len && bytes[i + 1] == b'|' => (2, 0),
            // bare `&` or `|`, `;`, newline → 1-char boundary
            b'&' | b'|' | b';' | b'\n' => (1, 0),
            _ => {
                i += 1;
                continue;
            }
        };
        let seg = &command[start..i];
        let trimmed = seg.trim().to_string();
        if !trimmed.is_empty() {
            segments.push(trimmed);
        }
        i += boundary_len + skip_extra;
        start = i;
    }

    // Push trailing segment
    let tail = command[start..].trim();
    if !tail.is_empty() {
        segments.push(tail.to_string());
    }

    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_network_command_heads() {
        for c in [
            "git clone https://x",
            "git -C repo fetch origin",
            "git fetch origin",
            "git push",
            "git ls-remote origin",
            "git remote update",    // `update` form hits the network
            "git submodule update", // fetches submodules
            "curl https://x",
            "wget http://x",
            "nc 10.0.0.1 80",
            "ssh host",
            "scp a b:c",
            "apk add git",
            "apk --update-cache search nodejs",
            "npm install",
            "npm --prefix app ci",
            "npm create vite@latest . -- --template react --no-interactive",
            "npx create-next-app demo",
            "pip install requests",
            "pip3 download black",
            "python3 -m pip install requests",
            "echo hi && git pull", // any segment counts
            "ls | curl x",
        ] {
            assert!(network_intent(c).is_some(), "{c:?} should be flagged");
        }
    }

    #[test]
    fn advisory_mentions_guest_gate_and_ish_limitation() {
        let advice = network_intent("npm install").expect("network intent should be detected");
        assert!(advice.contains("approved shell invocation"), "{advice}");
        assert!(advice.contains("iSH/OpenMinis"), "{advice}");
    }

    #[test]
    fn flags_install_aliases_and_prefixed_commands() {
        // `npm i` is npm's canonical install shorthand and the form an agent
        // most often emits; only the long `install` spelling was detected.
        for command in [
            "npm i left-pad",
            "npm init vite@latest",
            "npm in left-pad",
            "npm add left-pad",
            "npm up",
            "npm exec cowsay",
        ] {
            assert!(
                network_intent(command).is_some(),
                "{command:?} should be flagged as network intent"
            );
        }
        // A leading assignment or a transparent wrapper hid the real head.
        for command in [
            "NODE_ENV=production npm install",
            "http_proxy=http://x curl https://example.com",
            "sudo apk add git",
            "env GIT_TERMINAL_PROMPT=0 git clone https://example.com/r.git",
            "nohup wget https://example.com",
            "sudo -u nobody curl https://example.com",
        ] {
            assert!(
                network_intent(command).is_some(),
                "{command:?} should be flagged as network intent"
            );
        }
    }

    #[test]
    fn prefer_offline_still_requires_network_approval() {
        assert!(
            network_intent("npm install --prefer-offline left-pad").is_some(),
            "--prefer-offline may still fetch packages on a cache miss"
        );
        assert!(
            network_intent("npm install --offline left-pad").is_none(),
            "--offline must remain classified as a no-network npm invocation"
        );
    }

    #[test]
    fn env_assignment_detection_does_not_swallow_real_heads() {
        // `--flag=value` and `a.b=c` are not shell assignments, so a head that
        // merely contains `=` must still be classified rather than skipped.
        assert!(!is_env_assignment("--registry=https://example.com"));
        assert!(!is_env_assignment("1BAD=x"));
        assert!(!is_env_assignment("a.b=c"));
        assert!(is_env_assignment("NODE_ENV=production"));
        assert!(is_env_assignment("_x1=y"));
        // The wrapper skip must not turn an otherwise-local command into a hit.
        assert!(network_intent("NODE_ENV=production npm run build").is_none());
        assert!(network_intent("sudo ls -la").is_none());
    }

    #[test]
    fn allows_local_commands() {
        for c in [
            "echo hi",
            "ls -la",
            "git status",
            "git diff",
            "git commit -m x", // local git is allowed (P4 git runs deny-net)
            "git remote -v",   // local: list remotes (NOT the `update` form)
            "git remote add origin url", // local: configures a remote
            "git submodule status", // local: no `update`
            "apk --no-network add git",
            "npm --offline install",
            "npx --no-install next build",
            "pip install --no-index ./dist/pkg.whl",
            "python3 -m pip install --no-index ./dist/pkg.whl",
            "grep -r foo .",
            "cat file | sed s/a/b/",
        ] {
            assert!(network_intent(c).is_none(), "{c:?} should be allowed");
        }
    }

    #[test]
    fn parse_failure_is_not_a_network_grant() {
        // Unparseable / weird input must NOT silently pass as "local"; the
        // caller treats `None` as "no detected net intent, run deny-net anyway"
        // — which is safe because the runner is deny-net regardless. Document
        // that the advisory is best-effort and the seccomp filter is the real
        // stop. (No assertion beyond: does not panic.)
        let _ = network_intent("$(");
    }
}
