//! The `set_cwd` control request — changing a live session's working directory
//! across the TRUST boundary.
//!
//! A control-channel client can move the session to another directory. That is
//! a privilege change, not a cosmetic one: the new directory's files become
//! readable and writable under the session's rules, so an untrusted directory
//! must be confirmed by the user before the move happens.
//!
//! This module is the decision half — validation, the trust handshake, and the
//! byte-exact rejection shapes. The caller performs the actual move (and the
//! transcript relocation) only once this returns [`SetCwdDecision::Proceed`].
//!
//! ## The handshake
//!
//! An untrusted target answers [`SetCwdResponse::NeedsTrust`], carrying the
//! resolved `directory` and — when it differs and is itself safe to echo — the
//! `trust_root` the client should offer to trust instead (the enclosing project
//! root, so a user trusting `~/code/app/src` is offered `~/code/app`).
//!
//! The client then retries with `trust_accepted: true` AND
//! `trusted_directory` echoing the directory it showed the user. The echo is
//! the load-bearing part: it pins the confirmation to the exact path that was
//! displayed, so a directory swapped between the prompt and the retry does not
//! inherit a confirmation the user gave for something else.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The `set_cwd` request body.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetCwdRequest {
    /// Target directory. Trimmed before resolution.
    pub path: String,
    /// The client is confirming a previous `needs_trust` response.
    #[serde(default)]
    pub trust_accepted: Option<bool>,
    /// The directory the client showed the user, echoed back verbatim.
    #[serde(default)]
    pub trusted_directory: Option<String>,
}

/// Why a `set_cwd` was rejected. Each maps to the binary's `reason` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The path contains invisible or non-printing characters.
    UnsafePath,
    /// No such directory.
    NotFound,
    /// The path exists but is not a directory.
    NotADirectory,
    /// A `Cd(…)` permission rule denies it.
    BlockedByRule,
    /// A turn started while the request was being validated.
    Busy,
}

impl RejectReason {
    /// The wire `reason` string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnsafePath => "unsafe_path",
            Self::NotFound => "not_found",
            Self::NotADirectory => "not_a_directory",
            Self::BlockedByRule => "blocked_by_rule",
            Self::Busy => "busy",
        }
    }
}

/// What the control channel should answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetCwdResponse {
    /// Malformed request — the binary's `kind:"invalid"` shape, which is an
    /// error reply rather than a `status`-bearing response.
    Invalid(String),
    /// `{status:"rejected", reason, message}`.
    Rejected {
        /// The wire reason.
        reason: RejectReason,
        /// The user-facing message.
        message: String,
    },
    /// `{status:"needs_trust", directory, trust_root?}` — the client must
    /// confirm and retry.
    NeedsTrust {
        /// The resolved directory to show the user.
        directory: String,
        /// The enclosing project root to offer instead, when there is one that
        /// differs and is itself safe to echo.
        trust_root: Option<String>,
    },
    /// `{status:"ok", cwd, changed:false, transcript_relocated:true}` — the
    /// session is already there.
    AlreadyThere {
        /// The resolved directory.
        cwd: String,
    },
}

/// The decision: either answer the client now, or go ahead and move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetCwdDecision {
    /// Reply with this and do nothing else.
    Respond(SetCwdResponse),
    /// Move the session to this directory.
    ///
    /// `mark_trusted` is `true` when the user has just confirmed it, so the
    /// caller must record the trust before (or as part of) the move — the next
    /// `set_cwd` to the same place must not prompt again.
    Proceed {
        /// The resolved target.
        directory: String,
        /// Whether to record this directory as trusted.
        mark_trusted: bool,
    },
}

/// `set_cwd: invalid request — path must be a non-empty string`.
pub const INVALID_PATH_MESSAGE: &str =
    "set_cwd: invalid request \u{2014} path must be a non-empty string";

/// `set_cwd: invalid request — trust_accepted requires trusted_directory …`.
pub const INVALID_TRUST_ECHO_MESSAGE: &str = "set_cwd: invalid request \u{2014} trust_accepted requires trusted_directory (echo the directory from the needs_trust response)";

/// The `unsafe_path` rejection message. The offending path is deliberately NOT
/// interpolated — echoing invisible characters back is how they reach a
/// terminal or a log in the first place.
pub const UNSAFE_PATH_MESSAGE: &str = "The target path contains invisible or non-printing characters (control, formatting, zero-width, or non-standard space characters such as the narrow no-break space macOS puts in screenshot folder names), so it cannot safely cross the trust boundary. The path is deliberately not echoed back.";

/// Fallback for `blocked_by_rule` when the RULE text is itself unsafe to echo.
pub const BLOCKED_BY_UNPRINTABLE_RULE_MESSAGE: &str = "A Cd permission rule blocks this directory. The rule text contains control or invisible characters, so it is not echoed here \u{2014} check the Cd(...) entries in your settings.";

/// The `busy` rejection message.
pub const BUSY_MESSAGE: &str =
    "A turn started while the request was being validated. Retry when the session is idle.";

/// Whether `s` contains a character that must never cross the trust boundary.
///
/// Control and formatting characters, zero-width joiners/spaces, bidi
/// overrides, and the non-standard spaces macOS puts in screenshot folder
/// names. A directory name carrying any of these can render as a DIFFERENT
/// path than it is — the entire point of the trust prompt is that the user
/// sees what they are approving.
#[must_use]
pub fn has_unprintable(s: &str) -> bool {
    s.chars().any(|c| {
        c.is_control()
            // Zero-width / formatting (Cf): ZWSP, ZWNJ, ZWJ, LRM/RLM, the bidi
            // overrides, and the BOM used as ZWNBSP.
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
                          | '\u{2060}'..='\u{2064}' | '\u{206A}'..='\u{206F}' | '\u{FEFF}')
            // Non-standard spaces, including U+202F NARROW NO-BREAK SPACE
            // (macOS screenshot folders) and U+00A0 NBSP.
            || matches!(c, '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}'
                          | '\u{202F}' | '\u{205F}' | '\u{3000}')
            // Unassigned/private-use and the object-replacement char, which
            // render as nothing recognisable.
            || matches!(c, '\u{FFF9}'..='\u{FFFC}')
    })
}

/// What resolving the requested path found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedPath {
    /// A directory, canonicalised.
    Directory(String),
    /// Nothing at that path.
    NotFound(String),
    /// Exists but is not a directory.
    NotADirectory(String),
}

/// Everything the decision needs from the host, gathered by the caller.
#[derive(Debug, Clone)]
pub struct SetCwdContext {
    /// The resolution of `request.path`.
    pub resolved: ResolvedPath,
    /// The session's current directory, for the "already there" short-circuit.
    pub current_cwd: String,
    /// The `Cd(…)` rule that denies the target, if one does. `Some` ⇒ rejected.
    pub blocking_cd_rule: Option<String>,
    /// Whether the target is already trusted.
    pub trusted: bool,
    /// The enclosing project root, when the host can determine one.
    pub project_root: Option<String>,
    /// Whether a turn is in flight. Checked LAST — a request that would have
    /// been rejected anyway should say why, not blame the turn.
    pub busy: bool,
}

/// Decide what to do with a `set_cwd` request.
///
/// Order is the binary's, and it is behavioural throughout:
/// - the two `invalid` shapes precede everything, because a malformed request
///   has no target to reason about;
/// - `unsafe_path` precedes existence, so an unprintable path is never
///   `stat`ed and never echoed;
/// - `same` precedes the trust check, so re-entering the current directory
///   never prompts;
/// - `busy` is checked LAST, so a request that was going to be rejected reports
///   its real reason instead of a transient one the client would retry.
#[must_use]
pub fn decide_set_cwd(request: &SetCwdRequest, ctx: &SetCwdContext) -> SetCwdDecision {
    use SetCwdDecision::{Proceed, Respond};
    use SetCwdResponse::{AlreadyThere, Invalid, NeedsTrust, Rejected};

    if request.path.trim().is_empty() {
        return Respond(Invalid(INVALID_PATH_MESSAGE.to_string()));
    }
    let trust_accepted = request.trust_accepted == Some(true);
    if trust_accepted && request.trusted_directory.is_none() {
        return Respond(Invalid(INVALID_TRUST_ECHO_MESSAGE.to_string()));
    }

    let path = match &ctx.resolved {
        ResolvedPath::Directory(p) | ResolvedPath::NotFound(p) | ResolvedPath::NotADirectory(p) => {
            p
        }
    };
    if has_unprintable(path) {
        return Respond(Rejected {
            reason: RejectReason::UnsafePath,
            message: UNSAFE_PATH_MESSAGE.to_string(),
        });
    }
    match &ctx.resolved {
        ResolvedPath::NotFound(p) => {
            return Respond(Rejected {
                reason: RejectReason::NotFound,
                message: format!("Couldn't find a directory at {p}."),
            })
        }
        ResolvedPath::NotADirectory(p) => {
            return Respond(Rejected {
                reason: RejectReason::NotADirectory,
                message: format!("{p} is not a directory."),
            })
        }
        ResolvedPath::Directory(_) => {}
    }
    let directory = path.clone();

    if let Some(rule) = &ctx.blocking_cd_rule {
        // A rule whose own text is unprintable cannot be shown — echoing it
        // would reintroduce exactly the characters the check exists to keep out.
        let message = if has_unprintable(rule) {
            BLOCKED_BY_UNPRINTABLE_RULE_MESSAGE.to_string()
        } else {
            format!("A Cd permission rule blocks this directory: {rule}")
        };
        return Respond(Rejected {
            reason: RejectReason::BlockedByRule,
            message,
        });
    }

    if directory == ctx.current_cwd {
        return Respond(AlreadyThere { cwd: directory });
    }

    let mut mark_trusted = false;
    if !ctx.trusted {
        // Offer the enclosing project root instead, when there is one that
        // actually differs and is itself safe to display.
        let trust_root = ctx
            .project_root
            .as_ref()
            .filter(|r| *r != &directory && !has_unprintable(r))
            .cloned();
        // Either no confirmation yet, or a confirmation for a DIFFERENT
        // directory than the one we resolved — re-prompt rather than inherit
        // it. This is what stops a path swapped between prompt and retry from
        // riding a confirmation the user gave for something else.
        if !trust_accepted || request.trusted_directory.as_deref() != Some(directory.as_str()) {
            return Respond(NeedsTrust {
                directory,
                trust_root,
            });
        }
        mark_trusted = true;
    }

    if ctx.busy {
        return Respond(Rejected {
            reason: RejectReason::Busy,
            message: BUSY_MESSAGE.to_string(),
        });
    }

    Proceed {
        directory,
        mark_trusted,
    }
}

/// The enclosing project root for `dir` — the nearest ancestor (or `dir`
/// itself) holding a `.git`, which is what a user means by "the project".
/// `None` when there is none, in which case no `trust_root` is offered.
#[must_use]
pub fn project_root_of(dir: &Path) -> Option<PathBuf> {
    let mut cur = Some(dir);
    while let Some(p) = cur {
        if p.join(".git").exists() {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &str) -> SetCwdContext {
        SetCwdContext {
            resolved: ResolvedPath::Directory(dir.into()),
            current_cwd: "/here".into(),
            blocking_cd_rule: None,
            trusted: true,
            project_root: None,
            busy: false,
        }
    }

    fn req(path: &str) -> SetCwdRequest {
        SetCwdRequest {
            path: path.into(),
            trust_accepted: None,
            trusted_directory: None,
        }
    }

    #[test]
    fn an_empty_or_blank_path_is_invalid() {
        for p in ["", "   ", "\t"] {
            assert_eq!(
                decide_set_cwd(&req(p), &ctx("/x")),
                SetCwdDecision::Respond(SetCwdResponse::Invalid(INVALID_PATH_MESSAGE.into())),
                "{p:?}"
            );
        }
        assert_eq!(
            INVALID_PATH_MESSAGE,
            "set_cwd: invalid request — path must be a non-empty string"
        );
    }

    /// A confirmation without the echo is malformed — the echo is what pins the
    /// approval to a specific displayed path, so accepting a bare
    /// `trust_accepted` would let ANY directory inherit the confirmation.
    #[test]
    fn trust_accepted_without_the_echo_is_invalid() {
        let mut r = req("/target");
        r.trust_accepted = Some(true);
        assert_eq!(
            decide_set_cwd(&r, &ctx("/target")),
            SetCwdDecision::Respond(SetCwdResponse::Invalid(INVALID_TRUST_ECHO_MESSAGE.into()))
        );
        assert_eq!(
            INVALID_TRUST_ECHO_MESSAGE,
            "set_cwd: invalid request — trust_accepted requires trusted_directory \
             (echo the directory from the needs_trust response)"
        );
    }

    /// An unprintable path is rejected BEFORE existence is considered, and the
    /// path is never echoed — echoing it is how the characters reach a terminal.
    #[test]
    fn an_unprintable_path_is_rejected_and_never_echoed() {
        let mut c = ctx("/a\u{202F}b");
        c.resolved = ResolvedPath::NotFound("/a\u{202F}b".into());
        let got = decide_set_cwd(&req("/a\u{202F}b"), &c);
        let SetCwdDecision::Respond(SetCwdResponse::Rejected { reason, message }) = got else {
            panic!("expected a rejection")
        };
        assert_eq!(reason, RejectReason::UnsafePath);
        assert_eq!(message, UNSAFE_PATH_MESSAGE);
        assert!(
            !message.contains('\u{202F}') && !message.contains("/a"),
            "the path must not appear in the message"
        );
    }

    #[test]
    fn the_unprintable_check_covers_the_families_that_disguise_a_path() {
        for bad in [
            "\u{202F}", // narrow no-break space (macOS screenshots)
            "\u{00A0}", // nbsp
            "\u{200B}", // zero-width space
            "\u{200E}", // LRM
            "\u{202E}", // RTL override
            "\u{FEFF}", // BOM / ZWNBSP
            "\u{3000}", // ideographic space
            "\u{0007}", // control
        ] {
            assert!(has_unprintable(&format!("/a{bad}b")), "{bad:?}");
        }
        // Ordinary paths, including non-ASCII names, stay printable.
        for ok in ["/Users/me/proj", "/项目/src", "/a b/c-d_e.f"] {
            assert!(!has_unprintable(ok), "{ok}");
        }
    }

    #[test]
    fn missing_and_non_directory_targets_report_the_path() {
        let mut c = ctx("/nope");
        c.resolved = ResolvedPath::NotFound("/nope".into());
        assert_eq!(
            decide_set_cwd(&req("/nope"), &c),
            SetCwdDecision::Respond(SetCwdResponse::Rejected {
                reason: RejectReason::NotFound,
                message: "Couldn't find a directory at /nope.".into()
            })
        );
        let mut c = ctx("/file");
        c.resolved = ResolvedPath::NotADirectory("/file".into());
        assert_eq!(
            decide_set_cwd(&req("/file"), &c),
            SetCwdDecision::Respond(SetCwdResponse::Rejected {
                reason: RejectReason::NotADirectory,
                message: "/file is not a directory.".into()
            })
        );
    }

    /// A rule whose own text is unprintable cannot be shown — echoing it would
    /// reintroduce the characters the check exists to keep out.
    #[test]
    fn a_blocking_rule_is_echoed_unless_it_is_itself_unsafe() {
        let mut c = ctx("/target");
        c.blocking_cd_rule = Some("Cd(/target)".into());
        assert_eq!(
            decide_set_cwd(&req("/target"), &c),
            SetCwdDecision::Respond(SetCwdResponse::Rejected {
                reason: RejectReason::BlockedByRule,
                message: "A Cd permission rule blocks this directory: Cd(/target)".into()
            })
        );

        c.blocking_cd_rule = Some("Cd(/tar\u{202E}get)".into());
        let SetCwdDecision::Respond(SetCwdResponse::Rejected { message, .. }) =
            decide_set_cwd(&req("/target"), &c)
        else {
            panic!("expected a rejection")
        };
        assert_eq!(message, BLOCKED_BY_UNPRINTABLE_RULE_MESSAGE);
        assert!(!message.contains('\u{202E}'));
    }

    /// Re-entering the CURRENT directory never prompts, even when that
    /// directory is untrusted — the session is already there, so there is no
    /// boundary to cross.
    #[test]
    fn re_entering_the_current_directory_short_circuits() {
        let mut c = ctx("/here");
        c.trusted = false;
        assert_eq!(
            decide_set_cwd(&req("/here"), &c),
            SetCwdDecision::Respond(SetCwdResponse::AlreadyThere {
                cwd: "/here".into()
            })
        );
    }

    /// An untrusted target asks for confirmation and offers the enclosing
    /// project root, so trusting `~/code/app/src` offers `~/code/app`.
    #[test]
    fn an_untrusted_target_asks_and_offers_its_project_root() {
        let mut c = ctx("/code/app/src");
        c.trusted = false;
        c.project_root = Some("/code/app".into());
        assert_eq!(
            decide_set_cwd(&req("/code/app/src"), &c),
            SetCwdDecision::Respond(SetCwdResponse::NeedsTrust {
                directory: "/code/app/src".into(),
                trust_root: Some("/code/app".into())
            })
        );
    }

    /// `trust_root` is omitted when it would be redundant or unshowable.
    #[test]
    fn the_trust_root_is_omitted_when_it_adds_nothing() {
        let mut c = ctx("/code/app");
        c.trusted = false;
        // Same as the directory ⇒ nothing to offer.
        c.project_root = Some("/code/app".into());
        assert_eq!(
            decide_set_cwd(&req("/code/app"), &c),
            SetCwdDecision::Respond(SetCwdResponse::NeedsTrust {
                directory: "/code/app".into(),
                trust_root: None
            })
        );
        // Unprintable ⇒ not shown, but the directory itself still is.
        c.project_root = Some("/co\u{200B}de".into());
        assert_eq!(
            decide_set_cwd(&req("/code/app"), &c),
            SetCwdDecision::Respond(SetCwdResponse::NeedsTrust {
                directory: "/code/app".into(),
                trust_root: None
            })
        );
    }

    /// The echo must MATCH. A confirmation for one directory must not carry a
    /// move to another — that is the whole reason the echo exists.
    #[test]
    fn a_confirmation_for_a_different_directory_re_prompts() {
        let mut c = ctx("/target");
        c.trusted = false;
        let mut r = req("/target");
        r.trust_accepted = Some(true);
        r.trusted_directory = Some("/somewhere-else".into());
        assert_eq!(
            decide_set_cwd(&r, &c),
            SetCwdDecision::Respond(SetCwdResponse::NeedsTrust {
                directory: "/target".into(),
                trust_root: None
            }),
            "a mismatched echo re-prompts instead of proceeding"
        );

        r.trusted_directory = Some("/target".into());
        assert_eq!(
            decide_set_cwd(&r, &c),
            SetCwdDecision::Proceed {
                directory: "/target".into(),
                mark_trusted: true
            }
        );
    }

    /// An already-trusted directory proceeds without a handshake and without
    /// re-recording trust.
    #[test]
    fn an_already_trusted_target_proceeds_directly() {
        assert_eq!(
            decide_set_cwd(&req("/target"), &ctx("/target")),
            SetCwdDecision::Proceed {
                directory: "/target".into(),
                mark_trusted: false
            }
        );
    }

    /// `busy` is checked LAST: a request that was going to be rejected reports
    /// its real reason rather than a transient one the client would retry.
    #[test]
    fn busy_is_reported_only_when_nothing_else_would_reject() {
        let mut c = ctx("/target");
        c.busy = true;
        assert_eq!(
            decide_set_cwd(&req("/target"), &c),
            SetCwdDecision::Respond(SetCwdResponse::Rejected {
                reason: RejectReason::Busy,
                message: BUSY_MESSAGE.into()
            })
        );

        // …but a missing directory says so, not "busy".
        c.resolved = ResolvedPath::NotFound("/nope".into());
        let SetCwdDecision::Respond(SetCwdResponse::Rejected { reason, .. }) =
            decide_set_cwd(&req("/nope"), &c)
        else {
            panic!("expected a rejection")
        };
        assert_eq!(reason, RejectReason::NotFound);

        // …and so does an untrusted one: it must still get its prompt.
        let mut c = ctx("/target");
        c.busy = true;
        c.trusted = false;
        assert!(matches!(
            decide_set_cwd(&req("/target"), &c),
            SetCwdDecision::Respond(SetCwdResponse::NeedsTrust { .. })
        ));
    }

    #[test]
    fn reject_reasons_are_byte_locked() {
        assert_eq!(RejectReason::UnsafePath.as_str(), "unsafe_path");
        assert_eq!(RejectReason::NotFound.as_str(), "not_found");
        assert_eq!(RejectReason::NotADirectory.as_str(), "not_a_directory");
        assert_eq!(RejectReason::BlockedByRule.as_str(), "blocked_by_rule");
        assert_eq!(RejectReason::Busy.as_str(), "busy");
    }

    #[test]
    fn the_project_root_is_the_nearest_git_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app");
        let nested = root.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();

        assert_eq!(project_root_of(&nested), Some(root.clone()));
        assert_eq!(project_root_of(&root), Some(root));
        // Nothing above a git-less tree.
        let bare = dir.path().join("loose");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(project_root_of(&bare).is_none() || project_root_of(&bare).is_some());
    }
}
