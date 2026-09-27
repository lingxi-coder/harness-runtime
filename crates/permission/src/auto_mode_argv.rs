//! WIZARD-06 — the `auto-mode-setup` FLAG GRAMMAR and apply pipeline driver.
//!
//! Byte-parity with claude-code 2.1.220 `auto-mode-setup`. This module owns the
//! parsing + ordering rules and the `--apply-file` driver; it deliberately owns
//! NO surface-specific behaviour (no exit codes, no printing), because the same
//! grammar backs two surfaces: the `lingxi-cli auto-mode-setup` subcommand and
//! the `/auto-mode-setup` slash command. It lives here, below both, so neither
//! surface can drift from the other's parsing — a lenient grammar on one side
//! would be a real hazard, since this command writes permission settings.
//!
//! Every grammar message + telemetry code below is byte-verified against the
//! 2.1.220 binary. Ordering precedence uses a left-to-right, first-violation-
//! wins walk over the canonical flag order (`--request-id` < `--apply-target` <
//! `--expect-sha256` < `--apply-file`); each specific message is emitted ONLY in
//! the structural situation its text describes, and flag arrangements the
//! binary's strings don't unambiguously attribute fall back to a `usage` error
//! carrying the oracle's generic [`PARSE_FALLBACK`] message rather than a
//! guessed one.
//!
//! The non-interactive propose entry (`--wizard posture=… scope=… depth=…
//! --propose`) is a whole-string grammar of its own and is matched first; its
//! answer values are validated against the sets the wizard actually offers,
//! because those answers authorise how far the recon reaches.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::auto_mode_setup as pipeline;
use crate::{persist_auto_mode_save, PermissionPaths, PermissionUpdateDestination, PersistError};

// ── telemetry codes (auto_mode_setup_write) ──────────────────────────────────

/// The oracle's `Nj` usage block, printed by every `usage` outcome.
///
/// Byte-exact from 2.1.220 apart from one rebrand: "the Claude config dir"
/// reads "the LingXi config dir", because that is the directory this build
/// actually contains a proposal to.
pub const USAGE: &str = "Usage:
  /auto-mode-setup [--request-id <uuid>] --wizard posture=<personal|open-source|enterprise|mixed> scope=<all|project> depth=<both|shell|repos|here> --propose
  /auto-mode-setup [--request-id <uuid>] [--apply-target <user|project>] --expect-sha256 <64-hex> --apply-file <absolute-path>   (reads a proposal JSON from a file under the system temp dir or the LingXi config dir \u{2014} the caller must have shown it to the user first; --expect-sha256 is required and the apply refuses unless the file\u{2019}s exact bytes hash to the given sha256)

--request-id must come first when used. The token must be a UUID (canonical 8-4-4-4-12 hex-and-dash form, either case) and is echoed verbatim as \"requestId\" on the command's JSON result, so a host with several commands in flight can match replies to requests.

--apply-target doesn\u{2019}t change where the config is written \u{2014} entries always land in the user settings file. It refuses a proposal whose scope answer doesn\u{2019}t match the save choice (user \u{2194} scope=all, project \u{2194} scope=project). Flags ride in the order shown; everything after --apply-file is the path.";

/// A generic usage / help outcome (`usage`).
pub const CODE_USAGE: &str = "usage";
/// A flag-ordering / flag-shape rejection (`bad_flag_grammar`).
pub const CODE_BAD_FLAG_GRAMMAR: &str = "bad_flag_grammar";

// ── byte-exact grammar messages (2.1.220) ────────────────────────────────────

/// `--apply-target` value was neither `user` nor `project`.
pub const APPLY_TARGET_BAD_VALUE: &str = "--apply-target must be \"user\" or \"project\".";
/// `--apply-target` appeared more than once.
pub const APPLY_TARGET_TWICE: &str =
    "--apply-target was given more than once \u{2014} pass exactly one.";
/// `--apply-target` was given but there is no `--apply-file` to apply to.
pub const APPLY_TARGET_ONLY_APPLY_FILE: &str = "--apply-target only applies to --apply-file.";
/// `--apply-target` appeared after `--apply-file`.
pub const APPLY_TARGET_AFTER_APPLY_FILE: &str =
    "--apply-target must come before --expect-sha256 and --apply-file \u{2014} not after --apply-file.";
/// `--request-id` appeared more than once.
pub const REQUEST_ID_TWICE: &str =
    "--request-id was given more than once \u{2014} pass exactly one, as the first flag.";
/// `--request-id` appeared after another flag (but before `--apply-file`).
pub const REQUEST_ID_MUST_COME_FIRST: &str =
    "--request-id must come first, before --apply-target and --expect-sha256.";
/// `--request-id` appeared after `--apply-file`.
pub const REQUEST_ID_AFTER_APPLY_FILE: &str =
    "--request-id must come first, before --expect-sha256 and --apply-file \u{2014} not after --apply-file.";
/// `--expect-sha256=<v>` (`=` form) instead of a space-separated value.
pub const EXPECT_EQ_FORM: &str =
    "--expect-sha256 takes its value space-separated, not with `=`: --expect-sha256 <64-hex> --apply-file <path>.";
/// `--expect-sha256` with no following value token.
pub const EXPECT_NEEDS_VALUE: &str =
    "--expect-sha256 needs the 64-character hex sha256 of the proposal file\u{2019}s exact bytes.";
/// `--expect-sha256` not immediately before `--apply-file` (or no `--apply-file`).
pub const EXPECT_ONLY_APPLY_FILE: &str =
    "--expect-sha256 applies only to --apply-file and must come directly before it (--apply-target goes before --expect-sha256).";
/// `--expect-sha256` appeared after `--apply-file`.
pub const EXPECT_AFTER_APPLY_FILE: &str =
    "--expect-sha256 must come before --apply-file, not after it.";
/// `--apply-file` with no path.
pub const APPLY_FILE_NEEDS_PATH: &str = "--apply-file needs a path to the reviewed proposal JSON.";
/// Bare `--apply` (one-shot apply of model output with no review) is refused.
pub const ONE_SHOT_APPLY: &str =
    "One-shot --apply isn\u{2019}t available (it would write model output with no review). Use --propose, show the result to the user, then --apply-file <path>.";

// ── parsed result ────────────────────────────────────────────────────────────

/// The `--apply-target` save scope (`YQ_[target]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyTarget {
    /// `user` — the user settings tier.
    User,
    /// `project` — the project settings tier.
    Project,
}

impl ApplyTarget {
    /// The verbatim flag value (`"user"` / `"project"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ApplyTarget::User => "user",
            ApplyTarget::Project => "project",
        }
    }
}

/// `--request-id` was given with no value (`--request-id=` or followed by
/// another flag).
pub const REQUEST_ID_NEEDS_VALUE: &str = "--request-id needs a value.";
/// `--request-id` was given a value that is not a canonical UUID.
///
/// Note what the wording commits to: "the token is refused, not echoed". The
/// rejected id must NOT appear in the error output. A request id lands in
/// telemetry and logs, so echoing an arbitrary token back would turn this
/// message into a log-injection sink — hence a fixed string with no
/// interpolation slot.
pub const REQUEST_ID_NOT_UUID: &str = "--request-id must be a UUID in canonical 8-4-4-4-12 hex-and-dash form (either case) \u{2014} the token is refused, not echoed.";

/// Printed when no specific message attributes the flag arrangement.
pub const PARSE_FALLBACK: &str = "Couldn\u{2019}t parse arguments.";

/// The non-interactive propose entry point's grammar, as the oracle spells it.
/// The whole argument string must match this exactly.
pub const PROPOSE_GRAMMAR: &str = r"^--wizard posture=(\S+) scope=(\S+) depth=(\S+)\s+--propose$";

/// Accepted `posture=` values.
pub const POSTURE_VALUES: [&str; 4] = ["personal", "open-source", "enterprise", "mixed"];
/// Accepted `scope=` values.
pub const SCOPE_VALUES: [&str; 2] = ["all", "project"];
/// Accepted `depth=` values.
pub const DEPTH_VALUES: [&str; 4] = ["both", "shell", "repos", "here"];

/// A validated `--wizard … --propose` invocation: the three setup answers that
/// would otherwise come from the interactive questions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposeInvocation {
    /// `posture=` — one of [`POSTURE_VALUES`].
    pub posture: String,
    /// `scope=` — one of [`SCOPE_VALUES`].
    pub scope: String,
    /// `depth=` — one of [`DEPTH_VALUES`].
    pub depth: String,
    /// The `--request-id` echoed verbatim on the JSON result, when one was
    /// given. Validated as a canonical UUID before it gets here.
    pub request_id: Option<String>,
}

/// Is `s` a UUID in canonical 8-4-4-4-12 hex-and-dash form (either case)?
#[must_use]
pub fn is_canonical_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    b.iter().enumerate().all(|(i, c)| match i {
        8 | 13 | 18 | 23 => *c == b'-',
        _ => c.is_ascii_hexdigit(),
    })
}

/// A fully order-validated `--apply-file` invocation. `expect_sha256` is left
/// optional here: a MISSING hash is rejected downstream by the permission
/// pipeline's `missing_hash_arg` gate, keeping the hash-bind enforcement in one
/// place regardless of grammar leniency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyFileInvocation {
    /// `--request-id` (telemetry correlation id), if any.
    pub request_id: Option<String>,
    /// `--apply-target` scope, if any.
    pub apply_target: Option<ApplyTarget>,
    /// `--expect-sha256` value (still validated for 64-hex downstream).
    pub expect_sha256: Option<String>,
    /// `--apply-file` proposal path.
    pub apply_file: PathBuf,
}

/// The result of parsing `auto-mode-setup` flags for the apply path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoModeSetupInvocation {
    /// `--help` / `-h` — print usage.
    Help,
    /// A valid `--apply-file` invocation ready for the permission pipeline.
    ApplyFile(ApplyFileInvocation),
    /// A valid `--wizard … --propose` invocation.
    Propose(ProposeInvocation),
}

/// A grammar rejection: the byte-exact `auto_mode_setup_write` `code` +
/// user-facing `message`. An empty `message` means "no specific attributable
/// message — print usage/help".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarError {
    /// The telemetry code (`usage` / `bad_flag_grammar`).
    pub code: &'static str,
    /// The byte-exact message, or `""` to fall back to help.
    pub message: &'static str,
}

/// Build a [`CODE_BAD_FLAG_GRAMMAR`] rejection carrying `message`.
#[must_use]
pub fn grammar(message: &'static str) -> GrammarError {
    GrammarError {
        code: CODE_BAD_FLAG_GRAMMAR,
        message,
    }
}

/// A bare `usage` fallback (caller prints help). Used both for `--help` errors
/// and for flag arrangements the binary's strings don't unambiguously attribute.
/// The generic [`PARSE_FALLBACK`] rejection used when no specific message
/// attributes the flag arrangement.
#[must_use]
pub fn usage_fallback() -> GrammarError {
    GrammarError {
        code: CODE_USAGE,
        message: PARSE_FALLBACK,
    }
}

/// Match the `--wizard posture=… scope=… depth=… --propose` form.
///
/// [`PROPOSE_GRAMMAR`] anchors the WHOLE argument string, with single spaces
/// between the four leading tokens, so on a token vector that is exactly five
/// tokens in that order. Returns `None` when the shape does not match at all
/// (the caller then continues into the apply-path parser); returns
/// `Some(Err(..))` when the shape matches but an answer value is not one the
/// wizard offers.
/// A leading `--request-id` split off the front of the token stream.
struct PeeledRequestId<'a> {
    /// The validated id, or the grammar rejection it earned. `Ok(None)` when
    /// there was no leading `--request-id` at all.
    id: Result<Option<String>, GrammarError>,
    /// The tokens after the flag (all of `args` when there was no flag).
    rest: &'a [String],
}

/// Split a leading `--request-id <uuid>` (or `--request-id=<uuid>`) off the
/// front — the oracle's `dNd`, which runs before either form is matched.
///
/// Validation is deferred rather than raised: the caller decides whether a bad
/// id is attributable here (propose) or belongs to the flag walk (apply-file),
/// which has its own ordering diagnostics for the same token.
fn peel_leading_request_id(args: &[String]) -> PeeledRequestId<'_> {
    let no_flag = PeeledRequestId {
        id: Ok(None),
        rest: args,
    };
    let Some(first) = args.first() else {
        return no_flag;
    };
    let (name, inline) = split_eq(first);
    if name != "--request-id" {
        return no_flag;
    }
    // The value is inline after `=`, or the next token — but NOT when that token
    // is itself a flag (`\s+(?!--)`).
    let (value, consumed) = match inline {
        Some(v) => (v.to_string(), 1),
        None => match args.get(1) {
            Some(v) if !v.starts_with("--") => (v.clone(), 2),
            _ => (String::new(), 1),
        },
    };
    let rest = &args[consumed..];
    if value.is_empty() {
        return PeeledRequestId {
            id: Err(grammar(REQUEST_ID_NEEDS_VALUE)),
            rest,
        };
    }
    // The id reaches telemetry and logs, so it is constrained to a canonical
    // UUID and a bad token is refused WITHOUT being echoed.
    if !is_canonical_uuid(&value) {
        return PeeledRequestId {
            id: Err(grammar(REQUEST_ID_NOT_UUID)),
            rest,
        };
    }
    PeeledRequestId {
        id: Ok(Some(value)),
        rest,
    }
}

fn match_propose_form(args: &[String]) -> Option<Result<ProposeInvocation, GrammarError>> {
    if args.len() != 5 || args[0] != "--wizard" || args[4] != "--propose" {
        return None;
    }
    let posture = args[1].strip_prefix("posture=")?;
    let scope = args[2].strip_prefix("scope=")?;
    let depth = args[3].strip_prefix("depth=")?;
    // `(\S+)` — a value may not be empty or contain whitespace.
    if [posture, scope, depth]
        .iter()
        .any(|v| v.is_empty() || v.split_whitespace().count() != 1)
    {
        return Some(Err(usage_fallback()));
    }
    // Fail closed on an unoffered answer rather than passing it through: these
    // values authorise how far the recon reaches.
    if !POSTURE_VALUES.contains(&posture)
        || !SCOPE_VALUES.contains(&scope)
        || !DEPTH_VALUES.contains(&depth)
    {
        return Some(Err(usage_fallback()));
    }
    Some(Ok(ProposeInvocation {
        posture: posture.to_string(),
        scope: scope.to_string(),
        depth: depth.to_string(),
        // Filled in by the caller, which owns the peeled prefix.
        request_id: None,
    }))
}

/// Split `--flag=value` into (`--flag`, `Some(value)`); a bare flag is
/// (`--flag`, `None`).
fn split_eq(token: &str) -> (&str, Option<&str>) {
    if token.starts_with("--") {
        if let Some(eq) = token.find('=') {
            return (&token[..eq], Some(&token[eq + 1..]));
        }
    }
    (token, None)
}

/// Parse + order-validate `auto-mode-setup` apply-path flags (`args` are the
/// tokens after the subcommand name). See the module docs for the precedence
/// model. Returns [`AutoModeSetupInvocation`] on success or a byte-exact
/// [`GrammarError`].
///
/// # Errors
/// Returns [`GrammarError`] for any flag-shape or ordering violation.
pub fn parse_apply_file_args(args: &[String]) -> Result<AutoModeSetupInvocation, GrammarError> {
    // `Gay`: `if (t === "" || t === "--help" || t === "-h") return {mode:"usage",
    // message: Nj}` — a BARE invocation takes the same no-logCode branch as
    // `--help`, so it prints usage and records NO telemetry. Routing it through
    // `usage_fallback()` instead would both prepend "Couldn't parse arguments."
    // to a request that had nothing to parse and emit a `usage` event the
    // oracle never emits.
    if args.is_empty() {
        return Ok(AutoModeSetupInvocation::Help);
    }
    // The propose form is a whole-string grammar of its own, checked before the
    // flag walk so its tokens are never mistaken for apply-path flags.
    //
    // The oracle strips a leading `--request-id` (`dNd`) BEFORE matching either
    // form (`Gay`), so `--request-id <uuid> --wizard … --propose` is valid and
    // echoes the id. Peel the same prefix here; when what follows is not the
    // propose form, fall through to the walk with the ORIGINAL args so the
    // apply-path grammar and its error messages are untouched.
    let peeled = peel_leading_request_id(args);
    if let Some(result) = match_propose_form(peeled.rest) {
        // Only now is a malformed leading id attributable to the propose form;
        // on the apply path the walk below produces the more specific message.
        let request_id = peeled.id?;
        return result.map(|mut inv| {
            inv.request_id = request_id;
            AutoModeSetupInvocation::Propose(inv)
        });
    }

    let mut request_id: Option<String> = None;
    let mut apply_target: Option<ApplyTarget> = None;
    let mut expect_sha256: Option<String> = None;
    let mut apply_file: Option<String> = None;
    // `--expect-sha256` must be immediately followed by `--apply-file`.
    let mut expect_awaiting_file = false;

    let mut i = 0;
    while i < args.len() {
        let (name, inline) = split_eq(&args[i]);

        // Enforce "--expect-sha256 ... must come directly before --apply-file".
        if expect_awaiting_file && name != "--apply-file" {
            return Err(grammar(EXPECT_ONLY_APPLY_FILE));
        }

        match name {
            "--help" | "-h" => return Ok(AutoModeSetupInvocation::Help),
            "--apply" => {
                return Err(GrammarError {
                    code: CODE_USAGE,
                    message: ONE_SHOT_APPLY,
                })
            }
            "--request-id" => {
                if request_id.is_some() {
                    return Err(grammar(REQUEST_ID_TWICE));
                }
                if apply_file.is_some() {
                    return Err(grammar(REQUEST_ID_AFTER_APPLY_FILE));
                }
                if apply_target.is_some() || expect_sha256.is_some() {
                    return Err(grammar(REQUEST_ID_MUST_COME_FIRST));
                }
                // `^--request-id(?:=|\s+(?!--))(\S+)\s*` — the value is taken
                // inline after `=` or as the next token, but NOT when that token
                // is itself a flag.
                let value = if let Some(v) = inline {
                    i += 1;
                    v.to_string()
                } else if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                    i += 2;
                    args[i - 1].clone()
                } else {
                    i += 1;
                    String::new()
                };
                if value.is_empty() {
                    return Err(grammar(REQUEST_ID_NEEDS_VALUE));
                }
                // The id reaches telemetry and logs, so it is constrained to a
                // canonical UUID and a bad token is refused WITHOUT being echoed.
                if !is_canonical_uuid(&value) {
                    return Err(grammar(REQUEST_ID_NOT_UUID));
                }
                request_id = Some(value);
            }
            "--apply-target" => {
                if apply_file.is_some() {
                    return Err(grammar(APPLY_TARGET_AFTER_APPLY_FILE));
                }
                if apply_target.is_some() {
                    return Err(grammar(APPLY_TARGET_TWICE));
                }
                if expect_sha256.is_some() {
                    // After --expect-sha256 but before --apply-file: no binary
                    // message attributes this exactly → usage fallback.
                    return Err(usage_fallback());
                }
                let (value, next_i) = if let Some(v) = inline {
                    (v.to_string(), i + 1)
                } else if i + 1 < args.len() {
                    (args[i + 1].clone(), i + 2)
                } else {
                    (String::new(), i + 1)
                };
                apply_target = Some(match value.as_str() {
                    "user" => ApplyTarget::User,
                    "project" => ApplyTarget::Project,
                    _ => return Err(grammar(APPLY_TARGET_BAD_VALUE)),
                });
                i = next_i;
            }
            "--expect-sha256" => {
                if apply_file.is_some() {
                    return Err(grammar(EXPECT_AFTER_APPLY_FILE));
                }
                if expect_sha256.is_some() {
                    // Repeated --expect-sha256: no specific binary message.
                    return Err(usage_fallback());
                }
                if inline.is_some() {
                    return Err(grammar(EXPECT_EQ_FORM));
                }
                // `d === void 0 || d.startsWith("--")` — a following FLAG is not
                // taken as the digest. Without this the flag is consumed as the
                // hash and the walk reports a misleading ordering error instead
                // of "needs a value".
                if i + 1 >= args.len() || args[i + 1].is_empty() || args[i + 1].starts_with("--") {
                    return Err(grammar(EXPECT_NEEDS_VALUE));
                }
                expect_sha256 = Some(args[i + 1].clone());
                expect_awaiting_file = true;
                i += 2;
            }
            "--apply-file" => {
                let path = if let Some(v) = inline {
                    let p = v.to_string();
                    i += 1;
                    p
                } else if i + 1 < args.len() && !args[i + 1].is_empty() {
                    let p = args[i + 1].clone();
                    i += 2;
                    p
                } else {
                    return Err(grammar(APPLY_FILE_NEEDS_PATH));
                };
                if path.is_empty() {
                    return Err(grammar(APPLY_FILE_NEEDS_PATH));
                }
                apply_file = Some(path);
                expect_awaiting_file = false;
            }
            _ => return Err(usage_fallback()),
        }
    }

    // Post-walk: --expect-sha256 with no following --apply-file.
    if expect_awaiting_file {
        return Err(grammar(EXPECT_ONLY_APPLY_FILE));
    }
    let Some(apply_file) = apply_file else {
        // No --apply-file. Attribute a dangling --apply-target / --expect-sha256.
        if apply_target.is_some() {
            return Err(grammar(APPLY_TARGET_ONLY_APPLY_FILE));
        }
        if expect_sha256.is_some() {
            return Err(grammar(EXPECT_ONLY_APPLY_FILE));
        }
        return Err(usage_fallback());
    };

    Ok(AutoModeSetupInvocation::ApplyFile(ApplyFileInvocation {
        request_id,
        apply_target,
        expect_sha256,
        apply_file: PathBuf::from(apply_file),
    }))
}

// ── apply-file orchestration core ────────────────────────────────────────────

/// The result of running the `--apply-file` flow end-to-end. Gate rejections
/// carry the permission pipeline's OWN byte-exact `code` + `reason`; a
/// save-payload validation failure carries the byte-exact `vNs` reason (its
/// telemetry code is confirmed in the dispatch wave, so it is kept distinct
/// here rather than guessed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyResult {
    /// A pre-write gate rejected the request (`bad_path` / `read_denied` /
    /// `read_failed` / `too_large` / `missing_hash_arg` / `bad_hash_arg` /
    /// `hash_mismatch` / `parse_failed` / `scope_mismatch`).
    Rejected {
        /// The byte-exact `auto_mode_setup_write` code.
        code: String,
        /// The byte-exact user-facing reason.
        reason: String,
    },
    /// [`pipeline::validate_auto_mode_save`] rejected the payload; `reason` is
    /// byte-exact (e.g. "Nothing to save.").
    InvalidSave {
        /// The byte-exact `vNs` validation message.
        reason: String,
    },
    /// The settings file already matched — nothing was written.
    NoChange,
    /// The settings were written; `removed_count` `permissions.allow` entries
    /// were filtered out.
    Wrote {
        /// How many `permissions.allow` rules the removal set filtered out.
        removed_count: usize,
    },
}

/// Map `--apply-target` to the destination settings tier (default: user).
fn destination_for(target: Option<ApplyTarget>) -> PermissionUpdateDestination {
    match target {
        Some(ApplyTarget::Project) => PermissionUpdateDestination::ProjectSettings,
        _ => PermissionUpdateDestination::UserSettings,
    }
}

/// Read one proposal category (`environment` / `allow` / `soft_deny` /
/// `hard_deny`) as an array of values (empty when absent or non-array).
fn category(proposal: &Value, key: &str) -> Vec<Value> {
    proposal
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Run the `--apply-file` flow: secure read → pre-write gates
/// ([`pipeline::evaluate_apply_file`]) → build the `autoMode` block from the
/// proposal → validate the save payload → persist. The security-relevant gates
/// (path/read, hash-verify, parse, scope) all run inside `evaluate_apply_file`;
/// this function only sequences them and maps the proposal to the write.
///
/// `roots` are the temp/config containment roots, `is_read_denied` the live
/// policy's `Read`-deny predicate, `paths` the settings destination roots.
///
/// # Errors
/// Propagates [`PersistError`] from the settings write (broken-JSON destination
/// or a hardened-filesystem failure).
pub async fn execute_apply_file(
    inv: &ApplyFileInvocation,
    roots: &[PathBuf],
    is_read_denied: impl Fn(&Path) -> bool,
    paths: &PermissionPaths,
) -> Result<ApplyResult, PersistError> {
    let read = pipeline::read_proposal_file(&inv.apply_file);
    let scope = inv.apply_target.map(ApplyTarget::as_str);
    let args = pipeline::ApplyFileArgs {
        path: &inv.apply_file,
        roots,
        expect_sha256: inv.expect_sha256.as_deref(),
        apply_target: scope,
        expected_scope: scope,
    };
    let proposal = match pipeline::evaluate_apply_file(&args, is_read_denied, read) {
        pipeline::ApplyFilePipeline::Rejected { code, reason } => {
            return Ok(ApplyResult::Rejected { code, reason });
        }
        pipeline::ApplyFilePipeline::Proceed { proposal } => proposal,
    };

    let block = pipeline::build_auto_mode_settings(
        &category(&proposal, "environment"),
        &category(&proposal, "allow"),
        &category(&proposal, "soft_deny"),
        &category(&proposal, "hard_deny"),
    );
    let remove_value = proposal.get("remove_from_permissions_allow").cloned();

    if let Some(reason) = pipeline::validate_auto_mode_save(Some(&block), remove_value.as_ref()) {
        return Ok(ApplyResult::InvalidSave { reason });
    }

    let remove_strings: Vec<String> = remove_value
        .as_ref()
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    // The proposal's own `mode` decides whether this merges with the block
    // already on disk. It defaults to `append`, so a proposal that omits the
    // key must NOT clobber the user's existing auto-mode configuration.
    let mode = pipeline::AutoModeSaveMode::from_proposal(proposal.get("mode"));

    let outcome = persist_auto_mode_save(
        Some(&block),
        &remove_strings,
        destination_for(inv.apply_target),
        paths,
        mode,
    )
    .await?;

    Ok(if outcome.wrote {
        ApplyResult::Wrote {
            removed_count: outcome.removed_count,
        }
    } else {
        ApplyResult::NoChange
    })
}

/// The command's JSON result, as the oracle serialises it (`zay`).
///
/// `JSON.stringify(result, null, 2)`, with `requestId` merged in LAST when the
/// caller supplied one — a host with several commands in flight matches replies
/// to requests by that field, so it must survive verbatim.
#[must_use]
pub fn propose_result_json(body: Value, request_id: Option<&str>) -> String {
    let mut out = body;
    if let (Some(id), Some(map)) = (request_id, out.as_object_mut()) {
        map.insert("requestId".to_string(), Value::String(id.to_string()));
    }
    serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".to_string())
}

/// The result body for a completed propose run (oracle `Way`'s propose arm:
/// `{ok:true, proposal}` on success, `{ok:false, code, reason}` otherwise).
#[must_use]
pub fn propose_result_body(outcome: &crate::auto_mode_propose::ProposeOutcome) -> Value {
    match outcome {
        crate::auto_mode_propose::ProposeOutcome::Ok(success) => serde_json::json!({
            "ok": true,
            "proposal": success.proposal.to_json(),
        }),
        crate::auto_mode_propose::ProposeOutcome::Failed { code, reason, .. } => {
            serde_json::json!({
                "ok": false,
                "code": code,
                "reason": reason,
            })
        }
    }
}
