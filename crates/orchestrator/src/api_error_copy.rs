//! User-facing API-error copy — the `${IT}: …` family from claude-code's error
//! renderer (2.1.220 @230600350–230602900).
//!
//! Every string here was verified fragment by fragment against the binary, with
//! a control string that must NOT match: a pattern grep over a binary carrying
//! both UTF-8 and UTF-16LE text reports false positives without one.
//!
//! The separator between clauses is U+00B7 `·` (the binary's `\xB7`), not a
//! hyphen and not a bullet.
//!
//! # Scope
//!
//! Covered here: the 429 family, the billing/PTL bare strings, and the auth
//! family (credential rejection, cloud credentials, org-disabled, revoked
//! token, dead OAuth session). Still unported: the PDF-page and password
//! branches.
//!
//! ⚠️ Every user-facing auth instruction here names [`AUTH_COMMAND`]
//! (`/connect`), NOT the oracle's `/login` — see that constant.

use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Oracle `IT` — the prefix every rendered API error carries.
pub(crate) const API_ERROR: &str = "API Error";

/// Oracle `\xB7` — the clause separator. A hyphen here would be wrong.
const SEP: char = '·';

/// Oracle `Gcs`: does this message mean "the 1M-context window needs paid usage
/// credits"? Two accepted phrasings, matched verbatim.
#[must_use]
pub(crate) fn is_long_context_credit_message(message: &str) -> bool {
    message.contains("Extra usage is required for long context")
        || message.contains("Usage credits are required for long context")
}

/// Oracle: `${IT}: Usage credits required for 1M context · ${hint}`.
///
/// `non_interactive` picks the hint (`_n()`): an interactive session is told to
/// run the slash commands, a non-interactive one is pointed at the settings URL
/// and `--model`, because it has no slash commands to run.
#[must_use]
pub(crate) fn usage_credits_required_for_1m_context(non_interactive: bool) -> String {
    let hint = if non_interactive {
        format!("turn on usage credits at {USAGE_SETTINGS_URL}, or use --model to switch to standard context")
    } else {
        "run /usage-credits to turn them on, or /model to switch to standard context".to_string()
    };
    format!("{API_ERROR}: Usage credits required for 1M context {SEP} {hint}")
}

/// Oracle `xYr`.
const USAGE_SETTINGS_URL: &str = "claude.ai/settings/usage?from=cc_cli_limit_message";

/// Oracle `lir` — no usable credential, so the user has to sign in.
///
/// ⚠️ DELIBERATE DIVERGENCE — see [`AUTH_COMMAND`]. The oracle's bytes are
/// `Not logged in \xB7 Please run /login`.
pub(crate) const NOT_LOGGED_IN: &str = "Not logged in \u{b7} Please run /connect";

/// Oracle `cir` — a credential EXISTS but the server rejected it. "External"
/// because it came from outside the app: an env var or an `apiKeyHelper`
/// script, neither of which the auth command can fix.
pub(crate) const INVALID_API_KEY: &str = "Invalid API key \u{b7} Fix external API key";

/// Oracle `UOu` — an auth failure the client believes is transient.
pub(crate) const AUTH_TRANSIENT: &str =
    "Authentication error \u{b7} This may be a temporary network issue, please try again";

/// Is this a REMOTE session? — oracle `qOu()` = `Yt(process.env.CLAUDE_CODE_REMOTE)`.
///
/// Rebranded to `LINGXI_REMOTE`, matching the existing
/// `tool_ui::push_notification::is_remote` which ports the same predicate. This
/// is LingXi's OWN runtime flag, unlike the `CLAUDE_CODE_USE_*` provider
/// variables, which keep their names.
#[must_use]
pub(crate) fn is_remote_session() -> bool {
    lingxi_core::host::env::is_env_truthy(std::env::var("LINGXI_REMOTE").ok().as_deref())
}

/// Where the Anthropic credential came from — the oracle's `e1().source`,
/// kept at the granularity the error copy actually branches on.
///
/// A boolean was not enough: the org-disabled branch tells an env-var user and
/// an `apiKeyHelper` user to unset DIFFERENT things.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CredentialOrigin {
    /// An API key read from the environment, in the variable NAMED here.
    ///
    /// The oracle can say `ANTHROPIC_API_KEY` because claude-code only ever
    /// reads that one. LingXi resolves the variable per provider profile
    /// (`ProviderProfile::env_var`, the user's own `apiKeyEnv`), so the copy
    /// carries the name that was actually read — telling a Gemini user to unset
    /// `ANTHROPIC_API_KEY` is advice that cannot help them.
    ///
    /// This is why the type is no longer `Copy`.
    EnvApiKey {
        /// The environment variable the credential was read from.
        var: String,
    },
    /// An `apiKeyHelper` script.
    ApiKeyHelper,
    /// A managed key issued by signing in (the oracle calls this source
    /// `"/login managed key"`; the variant name follows the oracle's term).
    LoginManagedKey,
    /// Stored key, OAuth, or nothing — none of which the user "unsets".
    #[default]
    Other,
}

impl CredentialOrigin {
    /// Is this credential supplied from OUTSIDE the app, so signing in cannot
    /// fix it? Oracle: `source==="ANTHROPIC_API_KEY" || source==="apiKeyHelper"`.
    #[must_use]
    pub(crate) fn is_external(&self) -> bool {
        matches!(self, Self::EnvApiKey { .. } | Self::ApiKeyHelper)
    }
}

/// Oracle `re_`/`ne_`/`oe_`/`ie_` — the org has turned API-key auth off.
const ORG_DISABLED_PREFIX: &str = "Your organization has disabled API key authentication";

/// Oracle gate for that branch: a 403 naming the disablement.
#[must_use]
pub(crate) fn is_api_key_auth_disabled(status: Option<u16>, message: &str) -> bool {
    status == Some(403)
        && message
            .to_ascii_lowercase()
            .contains("api key authentication is disabled")
}

/// Which org-disabled remedy to offer.
///
/// ```js
/// if (i==="ANTHROPIC_API_KEY" && env.ANTHROPIC_API_KEY) return zv() ? re_ : ne_;
/// if (i==="apiKeyHelper") return oe_;
/// if (i==="/login managed key") return ie_;
/// ```
///
/// `has_oauth_token` is `zv()` (`ms()?.accessToken != null`): with an account
/// already signed in, unsetting the variable is enough; without one, the user
/// also has to sign in.
///
/// ⚠️ DELIBERATE DIVERGENCE — see [`AUTH_COMMAND`]. Every tail below that names
/// a command says `/connect`; the oracle says `/login`.
#[must_use]
pub(crate) fn api_key_auth_disabled_text(
    origin: &CredentialOrigin,
    has_oauth_token: bool,
    profile: Option<&str>,
) -> String {
    let account = account_display(profile);
    let tail = match origin {
        CredentialOrigin::EnvApiKey { var } if has_oauth_token => {
            format!("Unset {var} to use your {account} account instead")
        }
        CredentialOrigin::EnvApiKey { var } => {
            format!("Unset {var} and run {AUTH_COMMAND} to sign in with your {account} account")
        }
        CredentialOrigin::ApiKeyHelper => {
            format!(
                "Unset the apiKeyHelper setting and run {AUTH_COMMAND} to sign in with your \
                 {account} account"
            )
        }
        CredentialOrigin::LoginManagedKey | CredentialOrigin::Other => {
            format!("Run {AUTH_COMMAND} to sign in with your {account} account")
        }
    };
    format!("{ORG_DISABLED_PREFIX} \u{b7} {tail}")
}

/// Oracle `Epo` @230618923 — the GATEWAY could not authenticate upstream, so
/// nothing the user holds is wrong and no credential of theirs will fix it.
pub(crate) const GATEWAY_UPSTREAM_AUTH_FAILED: &str =
    "Authentication error \u{b7} The gateway could not authenticate with its upstream provider \
     \u{2014} contact your gateway administrator";

/// The auth command this product actually tells users to run.
///
/// ⚠️ **DELIBERATE DIVERGENCE from the oracle — do not "align" it back.**
/// claude-code says `/login`, which is Anthropic-specific. LingXi is a
/// MULTI-PROVIDER product, so every user-facing auth instruction names the
/// provider-neutral `/connect` instead. User decision, 2026-08-01.
///
/// A byte-parity audit will flag every string below as divergent; that is the
/// intended state. Record it as Divergence(multi-provider), not a gap.
pub(crate) const AUTH_COMMAND: &str = "/connect";

/// The product name to use when copy has to say WHOSE account is at fault.
///
/// ⚠️ DELIBERATE DIVERGENCE. The oracle hardcodes Claude because claude-code
/// only ever talks to Anthropic. LingXi routes to many providers, so the name
/// comes from the live session profile (`SessionState::model_profile`) — the
/// same source cost and telemetry attribution use.
///
/// The mapping deliberately mirrors the profile arm of
/// `llm_runtime::pricing_provider_id_for_profile` rather than introducing a
/// second table that could drift out of agreement with it. An unrecognised
/// profile names ITSELF instead of guessing a vendor: telling a `deepseek` user
/// about Claude is exactly the bug this exists to prevent.
///
/// ⚠️ Do NOT source this by parsing the model id — `split_profile_model`
/// answers `anthropic` for every bare non-`claude-` id, which would silently
/// restore the hardcoded behaviour.
#[must_use]
pub(crate) fn provider_display(profile: Option<&str>) -> String {
    match profile.unwrap_or("anthropic") {
        "anthropic" | "bedrock" | "foundry" => "Claude".to_string(),
        "openai" | "azure" => "OpenAI".to_string(),
        "gemini" | "vertex" => "Gemini".to_string(),
        other => other.to_string(),
    }
}

/// The ACCOUNT noun — what the user signs in to, which is not the product name.
///
/// The oracle writes "your claude.ai account", never "your Claude account", so
/// this is deliberately a second mapping rather than a reuse of
/// [`provider_display`]: `claude.ai` is the consumer account portal, `Claude` is
/// the product. Collapsing them would corrupt the Anthropic string, which is
/// otherwise byte-identical to the oracle.
#[must_use]
pub(crate) fn account_display(profile: Option<&str>) -> String {
    match profile.unwrap_or("anthropic") {
        "anthropic" | "bedrock" | "foundry" => "claude.ai".to_string(),
        "openai" | "azure" => "OpenAI".to_string(),
        "gemini" | "vertex" => "Google".to_string(),
        other => other.to_string(),
    }
}

/// Oracle `uir` — the interactive form of the revoked-token surface.
///
/// ⚠️ DELIBERATE DIVERGENCE — see [`AUTH_COMMAND`]. The oracle's bytes are
/// `OAuth token revoked \xB7 Please run /login`.
const OAUTH_TOKEN_REVOKED: &str = "OAuth token revoked \u{b7} Please run /connect";

/// Oracle's TERMINAL 401/403 arm — reached when no specific auth branch
/// matched (@230607344):
///
/// ```js
/// return yu({error:"authentication_failed", content: _n()
///   ? `Failed to authenticate. ${IT}: ${i}`
///   : `Please run /login \xB7 ${IT}: ${i}`})
/// ```
///
/// `i` is `sir(e)`, the oracle's error-text normalizer. For a real API error
/// with a body that is just `e.message` — i.e. the SDK's `${status} ${body}`,
/// which is what [`llm_runtime::LlmError::provider_message`] holds — so this is
/// byte-correct on the common path.
///
/// ⚠️ INCOMPLETE: `sir`'s SPECIAL cases are NOT ported (0 hits in this port) —
/// `StreamSuspended` → "Connection interrupted by system sleep", `ETIMEDOUT` →
/// "Request timed out. …", the seven SSL-code arms → "Unable to connect to
/// API: …", `"Connection error."` → "Unable to connect to API…", and the
/// empty-message fallback `API error (status …)`. Those need `sir` ported as
/// its own unit; note the port's [`llm_runtime::ssl::ssl_hint`] is a DIFFERENT
/// oracle function (`YLe`), not these strings.
///
/// ⚠️ DELIBERATE DIVERGENCE — see [`AUTH_COMMAND`].
#[must_use]
pub(crate) fn auth_failed_fallback(interactive: bool, detail: &str) -> String {
    if interactive {
        format!("Please run {AUTH_COMMAND} {SEP} {API_ERROR}: {detail}")
    } else {
        format!("Failed to authenticate. {API_ERROR}: {detail}")
    }
}

/// Oracle `ce_` (returned by `de_()`) — the org has switched OFF the Claude
/// SUBSCRIPTION path, so an API key is the way in.
///
/// Distinct from [`ORG_DISABLED_PREFIX`], which is the mirror image: that one
/// fires when the org disabled API-KEY auth and pushes the user toward signing
/// in. Getting them confused would tell a blocked user to do the exact thing
/// their org turned off.
///
/// ⚠️ DELIBERATE DIVERGENCE: the oracle names its own product ("for Claude
/// Code"); this port rebrands the product throughout its user copy. "Claude
/// subscription" is NOT rebranded — that names Anthropic's subscription, which
/// is what the org actually disabled.
pub(crate) const OAUTH_ORG_NOT_ALLOWED: &str =
    "Your organization has disabled Claude subscription access for LingXi \u{b7} \
     Use an Anthropic API key instead, or ask your admin to enable access";

/// Oracle gate: `(status===401||status===403)` AND the message names the block.
///
/// Both halves matter — the phrase is the server's, and the status range keeps
/// an unrelated 4xx carrying similar prose out of this branch.
#[must_use]
pub(crate) fn is_oauth_org_not_allowed(status: Option<u16>, message: &str) -> bool {
    matches!(status, Some(401 | 403))
        && message.contains("OAuth authentication is currently not allowed for this organization")
}

/// Oracle `Uke` — `status===403 && message.includes("OAuth token has been revoked")`.
///
/// Both halves matter: a 403 alone is an ordinary permission failure, and the
/// phrase alone could appear in unrelated text.
#[must_use]
pub(crate) fn is_oauth_revoked(status: Option<u16>, message: &str) -> bool {
    status == Some(403) && message.contains("OAuth token has been revoked")
}

/// Oracle `ue_()` — the revoked-token copy, split on interactivity.
///
/// The non-interactive half names a PRODUCT, and both the Anthropic and the
/// OpenAI credential providers funnel auth failures into the one renderer, so
/// it takes the live provider profile rather than hardcoding Claude the way the
/// oracle does. The interactive half names a command, not a product, and does
/// not vary.
#[must_use]
pub(crate) fn oauth_revoked_text(interactive: bool, profile: Option<&str>) -> String {
    if interactive {
        OAUTH_TOKEN_REVOKED.to_string()
    } else {
        format!(
            "Your account does not have access to {}. Please login again or \
             contact your administrator.",
            provider_display(profile)
        )
    }
}

/// Oracle `se_` — the interactive form of the dead-refresh-token surface.
///
/// ⚠️ DELIBERATE DIVERGENCE — see [`AUTH_COMMAND`]. The oracle's bytes are
/// `Login expired \xB7 Please run /login`.
const LOGIN_EXPIRED: &str = "Login expired \u{b7} Please run /connect";

/// Oracle's non-interactive twin, taken from the same render site
/// (`_n() ? … : se_`): a caller with no TTY cannot run the auth command, so the
/// copy states what happened instead of giving an instruction it cannot follow.
/// This half names no command, so it is byte-identical to the oracle.
const LOGIN_EXPIRED_NON_INTERACTIVE: &str =
    "Failed to authenticate: OAuth session expired and could not be refreshed";

/// Oracle's `e instanceof qQt` render arm, split on interactivity.
///
/// `qQt` is a distinct error CLASS (`OAuthRefreshDeadError`, "OAuth refresh
/// token is no longer valid; run /login to re-authenticate"), not a message
/// match — so the port keys on a distinct [`LlmError`] variant rather than
/// sniffing text. It fires only when the IdP actually REJECTED the refresh
/// token: a stale token hash or an unreachable IdP are different failures and
/// must not tell the user their login expired.
#[must_use]
pub(crate) fn oauth_refresh_dead_text(interactive: bool) -> &'static str {
    if interactive {
        LOGIN_EXPIRED
    } else {
        LOGIN_EXPIRED_NON_INTERACTIVE
    }
}

/// Does this error message name the `x-api-key` header?
///
/// The oracle's gate for the whole credential-rejection branch,
/// `message.toLowerCase().includes("x-api-key")` — it keys on the header the
/// server complained about, not on the status.
#[must_use]
pub(crate) fn mentions_api_key_header(message: &str) -> bool {
    message.to_ascii_lowercase().contains("x-api-key")
}

/// Which credential-rejection copy to show — oracle:
///
/// ```js
/// let {source:i} = e1();
/// return i==="ANTHROPIC_API_KEY" || i==="apiKeyHelper" ? cir : lir;
/// ```
///
/// `external` means the credential came from outside the app (the env var or a
/// helper script). Everything else — stored keys, OAuth, nothing at all — gets
/// the sign-in copy, because signing in is the fix.
#[must_use]
pub(crate) fn credential_rejected_text(origin: &CredentialOrigin) -> &'static str {
    if origin.is_external() {
        INVALID_API_KEY
    } else {
        NOT_LOGGED_IN
    }
}

/// Oracle `GOu` / `VOu` / `zOu` / `KOu` — cloud-provider credential failures.
const AWS_CREDS_EXPIRED: &str = "AWS credentials expired or invalid";
const AWS_AUTH_FAILED: &str = "AWS authentication failed";
const GCP_CREDS_EXPIRED: &str = "Google Cloud credentials expired or invalid";
const GCP_AUTH_FAILED: &str = "Google Cloud authentication failed";
/// Appended when the AWS failure is NOT a plain 401 — a live credential that
/// still cannot reach the model is a permissions problem, not an expiry one.
const AWS_PERMISSIONS_HINT: &str =
    " \u{b7} if credentials are current, check AWS permissions and model access";
/// Oracle default for the GCP re-auth command.
const GCLOUD_ADC_LOGIN: &str = "gcloud auth application-default login";

/// Credential copy for a cloud-hosted route — oracle's AWS branch @230606080
/// and Google Cloud branch @230606640.
///
/// ```js
/// let c = e.status===401 && (l==="anthropicAws"||l==="mantle"), u = c?GOu:VOu,
///     d = c ? "" : " \xB7 if credentials are current, check AWS permissions and model access";
/// // GCP: d = e.status===401, p = d?zOu:KOu, f = ` \xB7 run \`${u}\` and retry`
/// ```
///
/// The 401-vs-other split is only decidable because the status survives into the
/// message (`llm_runtime::api_error_status`); before that both collapsed to one
/// variant and this could not have been ported.
///
/// `None` for routes that are not cloud-hosted — the caller falls through to the
/// credential-rejection copy.
#[must_use]
pub(crate) fn cloud_credential_text(route: &ErrorRouteTag, status: Option<u16>) -> Option<String> {
    let is_401 = status == Some(401);
    match route {
        // `anthropicAws` and `mantle` take the expiry wording on a 401; Bedrock
        // reaches this branch too but never satisfies `l==="anthropicAws"`, so it
        // always gets the generic failure plus the permissions hint.
        ErrorRouteTag::AnthropicAws => Some(if is_401 {
            AWS_CREDS_EXPIRED.to_string()
        } else {
            format!("{AWS_AUTH_FAILED}{AWS_PERMISSIONS_HINT}")
        }),
        ErrorRouteTag::Other { display } if display == "Bedrock" || display == "Mantle" => {
            let expiry = is_401 && display == "Mantle";
            Some(if expiry {
                AWS_CREDS_EXPIRED.to_string()
            } else {
                format!("{AWS_AUTH_FAILED}{AWS_PERMISSIONS_HINT}")
            })
        }
        ErrorRouteTag::AnthropicGoogleCloud => {
            let head = if is_401 {
                GCP_CREDS_EXPIRED
            } else {
                GCP_AUTH_FAILED
            };
            Some(format!("{head} \u{b7} run `{GCLOUD_ADC_LOGIN}` and retry"))
        }
        _ => None,
    }
}

/// Oracle `LYr` — the billing surface for `LlmError::QuotaExceeded`.
///
/// Rendered BARE: `yu({content:LYr,error:"billing_error"})` carries no
/// `API Error:` prefix, unlike the 429 family. Getting that wrong would be
/// invisible in review and wrong on screen.
pub(crate) const CREDIT_BALANCE_TOO_LOW: &str = "Credit balance is too low";

/// Oracle `Jq` — the prompt-too-long surface for `LlmError::ContextOverflow`.
///
/// Also bare: `yu({content:Jq,error:"invalid_request"})`.
pub(crate) const PROMPT_TOO_LONG: &str = "Prompt is too long";

/// Oracle `z$v` (cc-238.js @228441394) — the width budget `Fol` truncates the
/// compaction-failure detail to.
const COMPACT_FAILURE_DETAIL_WIDTH: usize = 300;

/// SC-04 / oracle `Fol` (2.1.238 @228433532, 0 hits in 2.1.220) — the
/// prompt-too-long surface when the automatic compaction that would have
/// rescued the turn failed:
///
/// ```js
/// function Fol(e){
///   if(e?.reason!=="error"||!e.detail)return;
///   return `${_V} \xB7 automatic compaction failed: `+Yl(e.detail,z$v,!0)
/// }
/// ```
///
/// `_V` is [`PROMPT_TOO_LONG`], the separator is U+00B7 (never a hyphen), and
/// the detail runs through `Yl(detail, 300, true)` — first line only, with an
/// ellipsis when a newline was cut, clamped to 300 display columns.
///
/// Both oracle call sites fall back to the bare `_V` when this returns
/// `undefined` (`ep({content:Fol(qn)??_V,error:"invalid_request",…})`
/// @228721216 and the reactive twin @228749977), which is exactly what the
/// `None` here means for the caller. An empty detail yields `None`
/// (`!e.detail`).
#[must_use]
pub(crate) fn automatic_compaction_failed_text(detail: &str) -> Option<String> {
    if detail.is_empty() {
        return None;
    }
    Some(format!(
        "{PROMPT_TOO_LONG} {SEP} automatic compaction failed: {}",
        truncate_first_line(detail, COMPACT_FAILURE_DETAIL_WIDTH)
    ))
}

/// Oracle `Yl(e,t,r=!1)` with `r=true` (cc-238.js @220255323):
///
/// ```js
/// function Yl(e,t,r=!1){let n=e;
///   if(r){let o=e.indexOf(`\n`);
///     if(o!==-1){if(n=e.substring(0,o),ar(n)+1>t)return is(`${n}…`,t);
///       return `${n}…`}}
///   if(ar(n)<=t)return n;
///   return is(n,t)}
/// ```
///
/// `ar` is the display width and `is` the grapheme-wise width clamp below. Note
/// the oracle appends the ellipsis BEFORE clamping in the over-long first-line
/// branch, so the ellipsis itself is what gets dropped — transcribed as-is.
fn truncate_first_line(text: &str, width: usize) -> String {
    if let Some(nl) = text.find('\n') {
        let head = &text[..nl];
        let ellipsized = format!("{head}\u{2026}");
        if head.width() + 1 > width {
            return clamp_to_width(&ellipsized, width);
        }
        return ellipsized;
    }
    if text.width() <= width {
        return text.to_string();
    }
    clamp_to_width(text, width)
}

/// Oracle `is(e,t)` (cc-238.js, immediately above `Yl`): keep whole grapheme
/// clusters while the accumulated display width stays within `t`.
///
/// ```js
/// let r=0,n="";for(let{segment:o}of H_().segment(e)){let i=ar(o);
///   if(r+i>t)break;n+=o,r+=i}return n
/// ```
fn clamp_to_width(text: &str, width: usize) -> String {
    let mut used = 0usize;
    let mut out = String::new();
    for cluster in text.graphemes(true) {
        let w = cluster.width();
        if used + w > width {
            break;
        }
        out.push_str(cluster);
        used += w;
    }
    out
}

/// Oracle `le_` — the first-party variant of the rejection label, used instead
/// of `Request rejected (429)` when the limit is the server's rather than the
/// account's.
///
/// Not selected yet: choosing between this and [`REQUEST_REJECTED_429`] needs
/// the provider-route signal the oracle's `i` carries, which this layer does
/// not have. Kept (and tested) so the string is already byte-verified when that
/// plumbing lands — deleting and re-deriving it later is how transcription
/// errors get in.
pub(crate) const SERVER_LIMITING: &str =
    "Server is temporarily limiting requests (not your usage limit)";

/// Recover the detail clause from a 429 message — oracle:
///
/// ```js
/// let c = e.message.replace(/^429\s+/,""), u;
/// try { let m = Ut(c), g = m?.error?.message ?? m?.message; if (typeof g==="string") u = g } catch {}
/// let d = u || c;
/// ```
///
/// Strip the status prefix, try to read the body as JSON and take
/// `error.message` (falling back to a top-level `message`), and use the stripped
/// remainder verbatim when it is not JSON. This only works because the decoder
/// stringifies the whole body into the message — see
/// `llm_runtime::providers::api_error_message`.
#[must_use]
pub(crate) fn rate_limit_detail(message: &str) -> String {
    let stripped = strip_status_prefix(message, 429);
    let parsed = serde_json::from_str::<Value>(stripped)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|s| !s.is_empty());
    parsed.unwrap_or_else(|| stripped.to_string())
}

/// `message.replace(/^{status}\s+/, "")` — strip the status and the whitespace
/// run after it. Returns the input unchanged when the prefix is absent.
fn strip_status_prefix(message: &str, status: u16) -> &str {
    let digits = status.to_string();
    let Some(rest) = message.strip_prefix(&digits) else {
        return message;
    };
    let trimmed = rest.trim_start_matches([' ', '\t', '\n', '\r']);
    // `\s+` requires at least one whitespace character; `"429x"` keeps its text.
    if trimmed.len() == rest.len() {
        return message;
    }
    trimmed
}

/// Oracle: `${IT}: ${label} · ${detail || fallback}`.
///
/// `label` is `Request rejected (429)` normally, or [`SERVER_LIMITING`] when the
/// throttle is the server's. `fallback` is the
/// `this may be a temporary capacity issue.{suffix}` clause, used only when the
/// detail is empty — the suffix is provider-dependent (oracle `hpo()`), so the
/// caller supplies it.
#[must_use]
pub(crate) fn rate_limited_text(message: &str, label: &str, fallback: &str) -> String {
    let detail = rate_limit_detail(message);
    let clause = if detail.is_empty() { fallback } else { &detail };
    format!("{API_ERROR}: {label} {SEP} {clause}")
}

/// The default rejection label — oracle's non-first-party branch.
pub(crate) const REQUEST_REJECTED_429: &str = "Request rejected (429)";

/// The fallback clause's stem. The oracle appends [`persistence_suffix`].
pub(crate) const TEMPORARY_CAPACITY: &str = "this may be a temporary capacity issue.";

/// Oracle `jcs`.
const STATUS_PAGE: &str = "https://status.claude.com";

/// Which upstream a request is routed to — the oracle's `xn()` tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorRoute<'a> {
    /// `"firstParty"`. `default_endpoint` is the oracle's `Yd()`: false means a
    /// custom `ANTHROPIC_BASE_URL`, i.e. an inference gateway.
    FirstParty {
        /// Whether the request goes to the official endpoint.
        default_endpoint: bool,
        /// The configured base URL, named when it is a gateway.
        base_url: &'a str,
    },
    /// `"anthropicAws"`.
    AnthropicAws,
    /// `"anthropicGoogleCloud"`.
    AnthropicGoogleCloud,
    /// Anything else; `display` is the oracle's `rK[e]` provider name.
    Other {
        /// Provider display name.
        display: &'a str,
    },
}

/// Owned twin of [`ErrorRoute`], for carrying a route on
/// [`crate::OrchestratorConfig`] (which cannot hold borrowed strings).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", tag = "route")]
pub enum ErrorRouteTag {
    /// `"firstParty"`.
    FirstParty {
        /// Whether the request goes to the official endpoint (oracle `Yd()`).
        default_endpoint: bool,
        /// Configured base URL, named when it is a gateway.
        base_url: String,
    },
    /// `"gateway"` — an inference gateway fronting the provider.
    ///
    /// Checked FIRST by [`Self::from_env`], ahead of every `CLAUDE_CODE_USE_*`
    /// variable, because `xn()` opens with `if (C_()) return "gateway"`.
    Gateway,
    /// `"anthropicAws"`.
    AnthropicAws,
    /// `"anthropicGoogleCloud"`.
    AnthropicGoogleCloud,
    /// Anything else; the oracle's `rK[e]` display name.
    Other {
        /// Provider display name.
        display: String,
    },
}

impl ErrorRouteTag {
    /// Resolve the route from the environment — oracle `xn()` @227682549:
    ///
    /// ```js
    /// function xn(){
    ///   if (C_()) return "gateway";          // ← FIRST, outranks every env var
    ///   return Z.CLAUDE_CODE_USE_BEDROCK?"bedrock"
    ///      : Z.CLAUDE_CODE_USE_FOUNDRY?"foundry"
    ///      : Z.CLAUDE_CODE_USE_ANTHROPIC_AWS?"anthropicAws"
    ///      : Z.CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD?"anthropicGoogleCloud"
    ///      : Z.CLAUDE_CODE_USE_MANTLE?"mantle"
    ///      : Z.CLAUDE_CODE_USE_VERTEX?"vertex"
    ///      : "firstParty";
    /// }
    /// ```
    ///
    /// ORDER MATTERS — it is a chain, not a set, so `USE_BEDROCK` wins over
    /// `USE_VERTEX` when both are set.
    ///
    /// ⚠️ The chain is NOT the whole function. `xn()` checks `C_()` FIRST and
    /// returns `"gateway"` ahead of every environment variable. This port has
    /// no gateway-auth state (`C_()` is `Mt.gatewayAuth`, a runtime credential
    /// object written by `U5e()`), so that arm is genuinely unreachable here —
    /// but an earlier version of this doc showed only the env chain, which
    /// reads as though the env vars were the top of the precedence. **If
    /// gateway auth is ever added, it belongs BEFORE this chain, not appended
    /// to it**, and the `Epo` copy
    /// (`Authentication error · The gateway could not authenticate with its
    /// upstream provider — contact your gateway administrator`, @230618923)
    /// becomes reachable at the same time.
    ///
    /// These keep their `CLAUDE_CODE_` names: they are the PROVIDER's variables,
    /// which the rebrand deliberately preserves (the same call sites already
    /// read them in `apps/cli` and `migrations`). Only LingXi's own variables
    /// take the `LINGXI_` prefix.
    ///
    /// `hpo()` only distinguishes firstParty / anthropicAws / anthropicGoogleCloud
    /// from "everything else", so the remaining tags collapse into
    /// [`Self::Other`] carrying their display name.
    #[must_use]
    pub fn from_env() -> Self {
        fn on(var: &str) -> bool {
            std::env::var(var).is_ok_and(|v| !v.is_empty() && v != "0" && v != "false")
        }
        // `xn()` opens with `if (C_()) return "gateway"`, ahead of the whole
        // env chain. `C_()` is `Mt.gatewayAuth`, which `CJi()` bootstraps from
        // the environment: `CLAUDE_CODE_USE_GATEWAY` plus BOTH
        // `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` (@228931780). With
        // either missing the oracle warns and IGNORES the flag, so the
        // conjunction is the gate, not the flag alone.
        if on("CLAUDE_CODE_USE_GATEWAY")
            && std::env::var("ANTHROPIC_BASE_URL").is_ok_and(|v| !v.is_empty())
            && std::env::var("ANTHROPIC_AUTH_TOKEN").is_ok_and(|v| !v.is_empty())
        {
            return Self::Gateway;
        }
        if on("CLAUDE_CODE_USE_BEDROCK") {
            return Self::Other {
                display: "Bedrock".to_string(),
            };
        }
        if on("CLAUDE_CODE_USE_FOUNDRY") {
            return Self::Other {
                display: "Foundry".to_string(),
            };
        }
        if on("CLAUDE_CODE_USE_ANTHROPIC_AWS") {
            return Self::AnthropicAws;
        }
        if on("CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD") {
            return Self::AnthropicGoogleCloud;
        }
        if on("CLAUDE_CODE_USE_MANTLE") {
            return Self::Other {
                display: "Mantle".to_string(),
            };
        }
        if on("CLAUDE_CODE_USE_VERTEX") {
            return Self::Other {
                display: "Vertex".to_string(),
            };
        }
        // firstParty. `Yd()` picks the status page over the gateway wording;
        // modelled as "no custom base URL is configured".
        let base_url = std::env::var("ANTHROPIC_BASE_URL").unwrap_or_default();
        Self::FirstParty {
            default_endpoint: base_url.is_empty(),
            base_url,
        }
    }

    /// Borrow this tag as an [`ErrorRoute`].
    #[must_use]
    pub(crate) fn as_route(&self) -> ErrorRoute<'_> {
        match self {
            Self::FirstParty {
                default_endpoint,
                base_url,
            } => ErrorRoute::FirstParty {
                default_endpoint: *default_endpoint,
                base_url,
            },
            Self::AnthropicAws => ErrorRoute::AnthropicAws,
            Self::AnthropicGoogleCloud => ErrorRoute::AnthropicGoogleCloud,
            // `hpo()` only splits firstParty / anthropicAws / anthropicGoogleCloud
            // from everything else, so gateway collapses into `Other` carrying
            // the oracle's own display name (`rK.gateway`, @227683906).
            Self::Gateway => ErrorRoute::Other {
                display: "Cloud gateway",
            },
            Self::Other { display } => ErrorRoute::Other { display },
        }
    }
}

/// The full fallback clause: [`TEMPORARY_CAPACITY`] plus [`persistence_suffix`].
///
/// `None` renders the stem ALONE. That is a documented divergence, not parity:
/// the oracle's `xn()` always resolves to some route, but LingXi's error layer
/// only knows one when a composition root supplies it, and an unattributed
/// suffix ("check your  service status.") would be worse than none.
#[must_use]
pub(crate) fn capacity_fallback(route: Option<&ErrorRouteTag>) -> String {
    match route {
        Some(tag) => format!("{TEMPORARY_CAPACITY}{}", persistence_suffix(tag.as_route())),
        None => TEMPORARY_CAPACITY.to_string(),
    }
}

/// Oracle `hpo()` — the clause appended to
/// [`TEMPORARY_CAPACITY`], naming where to look if the trouble persists.
///
/// EVERY branch starts with a leading space; the caller concatenates without
/// one. Dropping it silently joins two words.
///
/// ```js
/// if (xn()==="firstParty") {
///   if (Yd()) return ` If it persists, check ${jcs}.`;
///   return ` If it persists, check your inference gateway (${URL.parse(t)?.host||t}).`
/// }
/// if (…==="anthropicAws") return ` If it persists, check ${jcs}.`;
/// if (…==="anthropicGoogleCloud") return ` If it persists, check ${jcs} and Google Cloud's status page.`;
/// return ` If it persists, check your ${rK[e]} service status.`;
/// ```
#[must_use]
pub(crate) fn persistence_suffix(route: ErrorRoute<'_>) -> String {
    match route {
        ErrorRoute::FirstParty {
            default_endpoint: true,
            ..
        }
        | ErrorRoute::AnthropicAws => format!(" If it persists, check {STATUS_PAGE}."),
        ErrorRoute::FirstParty { base_url, .. } => {
            // `URL.parse(t)?.host || t` — the HOST when it parses, else the raw
            // string. `host` keeps the port, unlike `hostname`.
            let host = url::Url::parse(base_url)
                .ok()
                .and_then(|u| {
                    u.host_str().map(|h| match u.port() {
                        Some(p) => format!("{h}:{p}"),
                        None => h.to_string(),
                    })
                })
                .unwrap_or_else(|| base_url.to_string());
            format!(" If it persists, check your inference gateway ({host}).")
        }
        ErrorRoute::AnthropicGoogleCloud => {
            format!(" If it persists, check {STATUS_PAGE} and Google Cloud's status page.")
        }
        ErrorRoute::Other { display } => {
            format!(" If it persists, check your {display} service status.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SC-04: `Fol` — the composed prompt-too-long surface, byte for byte.
    #[test]
    fn the_automatic_compaction_failure_hint_is_byte_exact() {
        assert_eq!(
            automatic_compaction_failed_text("summarizer 500").unwrap(),
            "Prompt is too long \u{b7} automatic compaction failed: summarizer 500"
        );
        // The separator is U+00B7, not a hyphen or a bullet.
        assert!(automatic_compaction_failed_text("x")
            .unwrap()
            .contains('\u{b7}'));
        // `!e.detail` → no hint at all; the caller falls back to the bare `_V`.
        assert!(automatic_compaction_failed_text("").is_none());
    }

    /// SC-04: `Yl(detail, 300, true)` — first line only, ellipsised when a
    /// newline was cut, and clamped to 300 columns.
    #[test]
    fn the_compaction_failure_detail_is_first_line_only_and_clamped() {
        // A newline anywhere → the head plus U+2026 (never `...`).
        assert_eq!(
            automatic_compaction_failed_text("boom\nstack frame 1\nstack frame 2").unwrap(),
            "Prompt is too long \u{b7} automatic compaction failed: boom\u{2026}"
        );
        // No newline and within budget → verbatim, no ellipsis.
        let short = "a".repeat(COMPACT_FAILURE_DETAIL_WIDTH);
        assert_eq!(
            truncate_first_line(&short, COMPACT_FAILURE_DETAIL_WIDTH),
            short
        );
        // No newline, over budget → clamped to exactly 300 columns, no ellipsis
        // (`is(n,t)`, not `is(n+"…",t)`).
        let long = "a".repeat(COMPACT_FAILURE_DETAIL_WIDTH + 50);
        let clamped = truncate_first_line(&long, COMPACT_FAILURE_DETAIL_WIDTH);
        assert_eq!(clamped.width(), COMPACT_FAILURE_DETAIL_WIDTH);
        assert!(!clamped.ends_with('\u{2026}'));
        // First line over budget → the oracle appends the ellipsis and THEN
        // clamps, so the ellipsis is the character that gets dropped.
        let long_head = format!("{}\ntail", "b".repeat(COMPACT_FAILURE_DETAIL_WIDTH + 10));
        let cut = truncate_first_line(&long_head, COMPACT_FAILURE_DETAIL_WIDTH);
        assert_eq!(cut, "b".repeat(COMPACT_FAILURE_DETAIL_WIDTH));
        // Width is display columns, not chars: wide graphemes count double, and
        // clusters are never split.
        let wide = "宽".repeat(200);
        assert_eq!(
            truncate_first_line(&wide, COMPACT_FAILURE_DETAIL_WIDTH)
                .chars()
                .count(),
            COMPACT_FAILURE_DETAIL_WIDTH / 2
        );
    }

    /// Both of these render BARE — no `API Error:` prefix — which is the easy
    /// thing to get wrong when every neighbouring string has one.
    /// `xn()` is a CHAIN, not a set: with both Bedrock and Vertex set, Bedrock
    /// wins. A `match`-style port that checked them in any other order would
    /// name the wrong provider in the message.
    ///
    /// Env is process-global, so this runs the whole chain in ONE test rather
    /// than racing sibling tests that set the same variables.
    #[test]
    fn the_route_chain_resolves_in_the_oracle_order() {
        const VARS: [&str; 6] = [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_USE_ANTHROPIC_AWS",
            "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
            "CLAUDE_CODE_USE_MANTLE",
            "CLAUDE_CODE_USE_VERTEX",
        ];
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved: Vec<Option<String>> = VARS.iter().map(|v| std::env::var(v).ok()).collect();
        let saved_base = std::env::var("ANTHROPIC_BASE_URL").ok();
        for v in VARS {
            std::env::remove_var(v);
        }
        std::env::remove_var("ANTHROPIC_BASE_URL");

        // Nothing set → firstParty on the official endpoint.
        assert_eq!(
            ErrorRouteTag::from_env(),
            ErrorRouteTag::FirstParty {
                default_endpoint: true,
                base_url: String::new()
            }
        );

        // A custom base URL keeps the route but drops the default-endpoint flag.
        std::env::set_var("ANTHROPIC_BASE_URL", "https://gw.test:9000");
        assert_eq!(
            ErrorRouteTag::from_env(),
            ErrorRouteTag::FirstParty {
                default_endpoint: false,
                base_url: "https://gw.test:9000".to_string()
            }
        );
        std::env::remove_var("ANTHROPIC_BASE_URL");

        std::env::set_var("CLAUDE_CODE_USE_VERTEX", "1");
        assert!(matches!(
            ErrorRouteTag::from_env(),
            ErrorRouteTag::Other { .. }
        ));
        // Bedrock outranks Vertex — the chain order, not alphabetical.
        std::env::set_var("CLAUDE_CODE_USE_BEDROCK", "1");
        assert_eq!(
            ErrorRouteTag::from_env(),
            ErrorRouteTag::Other {
                display: "Bedrock".to_string()
            }
        );
        std::env::remove_var("CLAUDE_CODE_USE_BEDROCK");

        // anthropicAws outranks Vertex too, and is its OWN branch in `hpo()`.
        std::env::set_var("CLAUDE_CODE_USE_ANTHROPIC_AWS", "1");
        assert_eq!(ErrorRouteTag::from_env(), ErrorRouteTag::AnthropicAws);
        std::env::remove_var("CLAUDE_CODE_USE_ANTHROPIC_AWS");

        std::env::set_var("CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD", "1");
        assert_eq!(
            ErrorRouteTag::from_env(),
            ErrorRouteTag::AnthropicGoogleCloud
        );

        for (v, old) in VARS.iter().zip(saved) {
            match old {
                Some(x) => std::env::set_var(v, x),
                None => std::env::remove_var(v),
            }
        }
        match saved_base {
            Some(x) => std::env::set_var("ANTHROPIC_BASE_URL", x),
            None => std::env::remove_var("ANTHROPIC_BASE_URL"),
        }
    }

    #[test]
    fn the_capacity_fallback_joins_stem_and_suffix_without_a_gap() {
        assert_eq!(
            capacity_fallback(Some(&ErrorRouteTag::AnthropicAws)),
            "this may be a temporary capacity issue. If it persists, check https://status.claude.com."
        );
        // Unknown route → stem alone, never a dangling "check your  service".
        assert_eq!(
            capacity_fallback(None),
            "this may be a temporary capacity issue."
        );
    }

    #[test]
    fn the_persistence_suffix_is_byte_exact_on_every_branch() {
        assert_eq!(
            persistence_suffix(ErrorRoute::FirstParty {
                default_endpoint: true,
                base_url: ""
            }),
            " If it persists, check https://status.claude.com."
        );
        assert_eq!(
            persistence_suffix(ErrorRoute::AnthropicAws),
            " If it persists, check https://status.claude.com."
        );
        assert_eq!(
            persistence_suffix(ErrorRoute::AnthropicGoogleCloud),
            " If it persists, check https://status.claude.com and Google Cloud's status page."
        );
        assert_eq!(
            persistence_suffix(ErrorRoute::Other { display: "Bedrock" }),
            " If it persists, check your Bedrock service status."
        );
        // A custom base URL is named by HOST, not the whole URL.
        assert_eq!(
            persistence_suffix(ErrorRoute::FirstParty {
                default_endpoint: false,
                base_url: "https://gw.example.com/v1/messages"
            }),
            " If it persists, check your inference gateway (gw.example.com)."
        );
        // `URL.host` keeps the port (unlike `hostname`).
        assert_eq!(
            persistence_suffix(ErrorRoute::FirstParty {
                default_endpoint: false,
                base_url: "https://gw.example.com:8443/v1"
            }),
            " If it persists, check your inference gateway (gw.example.com:8443)."
        );
        // Unparseable → the raw string, per `|| t`.
        assert_eq!(
            persistence_suffix(ErrorRoute::FirstParty {
                default_endpoint: false,
                base_url: "not a url"
            }),
            " If it persists, check your inference gateway (not a url)."
        );
    }

    /// Every branch begins with a space — the caller concatenates without one,
    /// so losing it joins "issue." to "If".
    #[test]
    fn every_persistence_suffix_branch_leads_with_a_space() {
        for r in [
            ErrorRoute::FirstParty {
                default_endpoint: true,
                base_url: "",
            },
            ErrorRoute::FirstParty {
                default_endpoint: false,
                base_url: "https://x.test",
            },
            ErrorRoute::AnthropicAws,
            ErrorRoute::AnthropicGoogleCloud,
            ErrorRoute::Other { display: "X" },
        ] {
            let s = persistence_suffix(r);
            assert!(s.starts_with(' '), "missing leading space: {s:?}");
        }
    }

    /// These are BARE too, and use U+00B7 like the 429 clause separator.
    ///
    /// NOTE the binary stores the JS escape `\xB7`, not the encoded character,
    /// so verifying these needs the escaped form — grepping for the decoded `·`
    /// returns 0 and looks like the string is absent.
    #[test]
    fn the_credential_copy_is_byte_exact() {
        // `/connect`, not the oracle's `/login` — see `AUTH_COMMAND`.
        assert_eq!(NOT_LOGGED_IN, "Not logged in \u{b7} Please run /connect");
        assert_eq!(
            INVALID_API_KEY,
            "Invalid API key \u{b7} Fix external API key"
        );
        assert_eq!(
            AUTH_TRANSIENT,
            "Authentication error \u{b7} This may be a temporary network issue, please try again"
        );
        for s in [NOT_LOGGED_IN, INVALID_API_KEY, AUTH_TRANSIENT] {
            assert!(!s.starts_with(API_ERROR), "{s} renders bare");
            assert!(s.contains('\u{b7}'), "{s} separates with U+00B7");
        }
    }

    /// An externally supplied credential was rejected → tell the user to fix
    /// THAT, because signing in cannot. Anything else → the auth command.
    #[test]
    fn credential_copy_splits_on_where_the_key_came_from() {
        assert_eq!(
            credential_rejected_text(&CredentialOrigin::EnvApiKey {
                var: "ANTHROPIC_API_KEY".to_string()
            }),
            INVALID_API_KEY
        );
        assert_eq!(
            credential_rejected_text(&CredentialOrigin::ApiKeyHelper),
            INVALID_API_KEY
        );
        // A managed key or a stored/OAuth credential IS fixable by signing in.
        assert_eq!(
            credential_rejected_text(&CredentialOrigin::LoginManagedKey),
            NOT_LOGGED_IN
        );
        assert_eq!(
            credential_rejected_text(&CredentialOrigin::Other),
            NOT_LOGGED_IN
        );
    }

    /// Each origin is told to unset a DIFFERENT thing — the reason a boolean
    /// could not carry this and the field had to be widened.
    #[test]
    fn org_disabled_names_the_right_thing_to_unset() {
        assert_eq!(
            api_key_auth_disabled_text(
                &CredentialOrigin::EnvApiKey {
                    var: "ANTHROPIC_API_KEY".to_string()
                },
                true,
                Some("anthropic")
            ),
            "Your organization has disabled API key authentication \u{b7} Unset \
             ANTHROPIC_API_KEY to use your claude.ai account instead"
        );
        // No account signed in yet → also has to sign in. `/connect`, not the
        // oracle's `/login` — see `AUTH_COMMAND`.
        assert_eq!(
            api_key_auth_disabled_text(
                &CredentialOrigin::EnvApiKey {
                    var: "ANTHROPIC_API_KEY".to_string()
                },
                false,
                Some("anthropic")
            ),
            "Your organization has disabled API key authentication \u{b7} Unset \
             ANTHROPIC_API_KEY and run /connect to sign in with your claude.ai account"
        );
        assert_eq!(
            api_key_auth_disabled_text(&CredentialOrigin::ApiKeyHelper, true, Some("anthropic")),
            "Your organization has disabled API key authentication \u{b7} Unset the \
             apiKeyHelper setting and run /connect to sign in with your claude.ai account"
        );
        assert_eq!(
            api_key_auth_disabled_text(&CredentialOrigin::LoginManagedKey, true, Some("anthropic")),
            "Your organization has disabled API key authentication \u{b7} Run /connect \
             to sign in with your claude.ai account"
        );
    }

    #[test]
    fn the_org_disabled_gate_needs_a_403_and_the_phrase() {
        assert!(is_api_key_auth_disabled(
            Some(403),
            "403 API Key authentication is disabled for this organization"
        ));
        assert!(!is_api_key_auth_disabled(
            Some(401),
            "api key authentication is disabled"
        ));
        assert!(!is_api_key_auth_disabled(Some(403), "forbidden"));
    }

    /// On a remote session a rejected key is reported as possibly transient,
    /// BEFORE the source split — the remote host may just have lost the network.
    #[test]
    fn a_remote_session_reports_the_failure_as_transient() {
        let saved = std::env::var("LINGXI_REMOTE").ok();
        std::env::remove_var("LINGXI_REMOTE");
        assert!(!is_remote_session());
        std::env::set_var("LINGXI_REMOTE", "1");
        assert!(is_remote_session());
        std::env::set_var("LINGXI_REMOTE", "0");
        assert!(
            !is_remote_session(),
            "`0` is falsy, matching the oracle's `Yt`"
        );
        match saved {
            Some(v) => std::env::set_var("LINGXI_REMOTE", v),
            None => std::env::remove_var("LINGXI_REMOTE"),
        }
    }

    #[test]
    fn the_oauth_revoked_gate_needs_both_halves() {
        assert!(is_oauth_revoked(
            Some(403),
            "403 OAuth token has been revoked"
        ));
        // A 403 alone is an ordinary permission failure.
        assert!(!is_oauth_revoked(Some(403), "403 forbidden"));
        // The phrase alone, on another status, is not this case.
        assert!(!is_oauth_revoked(Some(401), "OAuth token has been revoked"));
        assert!(!is_oauth_revoked(None, "OAuth token has been revoked"));
    }

    #[test]
    fn the_revoked_copy_splits_on_interactivity() {
        assert_eq!(
            // `/connect`, NOT the oracle's `/login` — deliberate
            // multi-provider divergence, see `AUTH_COMMAND`.
            oauth_revoked_text(true, Some("anthropic")),
            "OAuth token revoked \u{b7} Please run /connect"
        );
        // Non-interactive callers cannot run the auth command, so it names
        // the admin instead.
        assert_eq!(
            oauth_revoked_text(false, Some("anthropic")),
            "Your account does not have access to Claude. Please login again or \
             contact your administrator."
        );
    }

    /// The oracle replaces the stringified body with the human message — the
    /// common path, since `api_error_message` embeds the whole body.
    #[test]
    fn the_api_error_detail_unwraps_the_stringified_body() {
        assert_eq!(
            llm_runtime::api_error_detail(
                r#"403 {"type":"error","error":{"type":"permission_error","message":"OAuth token has been revoked"}}"#
            ),
            "403 OAuth token has been revoked"
        );
        // `FOu` falls back to a top-level `message`.
        assert_eq!(
            llm_runtime::api_error_detail(r#"401 {"message":"bad key"}"#),
            "401 bad key"
        );
        // No status prefix → the extracted text alone.
        assert_eq!(
            llm_runtime::api_error_detail(r#"{"message":"plain"}"#),
            "plain"
        );
        // Nothing extractable → unchanged, as the oracle's final return does.
        assert_eq!(
            llm_runtime::api_error_detail(r#"403 {"nope":1}"#),
            r#"403 {"nope":1}"#
        );
        // No JSON at all → unchanged.
        assert_eq!(
            llm_runtime::api_error_detail("403 forbidden"),
            "403 forbidden"
        );
    }

    /// Serializes every test in this module that touches the process
    /// environment. ONE lock for the state, taken by ALL readers and writers —
    /// a lock only protects the tests that take it.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The gateway arm. `xn()` checks it FIRST, so it must beat every
    /// `CLAUDE_CODE_USE_*` variable — and the oracle IGNORES the flag unless
    /// both companion variables are present.
    #[test]
    fn the_gateway_route_outranks_the_env_chain_but_needs_all_three_vars() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved: Vec<(&str, Option<String>)> = [
            "CLAUDE_CODE_USE_GATEWAY",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
        ]
        .iter()
        .map(|&k| (k, std::env::var(k).ok()))
        .collect();
        for (k, _) in &saved {
            std::env::remove_var(k);
        }

        // All three present → Gateway, even with BEDROCK also set.
        std::env::set_var("CLAUDE_CODE_USE_GATEWAY", "1");
        std::env::set_var("ANTHROPIC_BASE_URL", "https://gw.example");
        std::env::set_var("ANTHROPIC_AUTH_TOKEN", "jwt");
        std::env::set_var("CLAUDE_CODE_USE_BEDROCK", "1");
        assert_eq!(ErrorRouteTag::from_env(), ErrorRouteTag::Gateway);

        // Missing a companion → the oracle warns and IGNORES the flag, so the
        // env chain resumes and Bedrock wins.
        std::env::remove_var("ANTHROPIC_AUTH_TOKEN");
        assert_ne!(ErrorRouteTag::from_env(), ErrorRouteTag::Gateway);

        for (k, v) in saved {
            match v {
                Some(value) => std::env::set_var(k, value),
                None => std::env::remove_var(k),
            }
        }
    }

    /// A gateway that cannot authenticate upstream is not the user's problem.
    #[test]
    fn the_gateway_copy_blames_the_gateway_not_the_user() {
        assert_eq!(
            GATEWAY_UPSTREAM_AUTH_FAILED,
            "Authentication error \u{b7} The gateway could not authenticate with its upstream \
             provider \u{2014} contact your gateway administrator"
        );
        // It must NOT tell the user to re-authenticate.
        assert!(!GATEWAY_UPSTREAM_AUTH_FAILED.contains(AUTH_COMMAND));
    }

    /// The terminal 401/403 arm, reached when no specific branch matched.
    #[test]
    fn the_auth_fallback_carries_the_api_error_detail() {
        assert_eq!(
            auth_failed_fallback(true, "403 {\"error\":\"nope\"}"),
            "Please run /connect \u{b7} API Error: 403 {\"error\":\"nope\"}"
        );
        // Non-interactive states the fact instead of naming a command.
        assert_eq!(
            auth_failed_fallback(false, "401 bad"),
            "Failed to authenticate. API Error: 401 bad"
        );
    }

    /// Oracle `de_()` → `ce_`, gated on a 401/403 whose message names the
    /// org-level OAuth block. Distinct from the API-KEY disablement below: this
    /// one says the SUBSCRIPTION path is off and an API key is the way in.
    #[test]
    fn the_org_oauth_block_has_its_own_copy_and_gate() {
        assert!(is_oauth_org_not_allowed(
            Some(401),
            "401 OAuth authentication is currently not allowed for this organization"
        ));
        assert!(is_oauth_org_not_allowed(
            Some(403),
            "403 OAuth authentication is currently not allowed for this organization"
        ));
        // The oracle's gate is `status===401||status===403` AND the phrase.
        assert!(!is_oauth_org_not_allowed(
            Some(429),
            "OAuth authentication is currently not allowed for this organization"
        ));
        assert!(!is_oauth_org_not_allowed(Some(403), "403 forbidden"));

        // Byte-exact but for the product name: the oracle says "Claude Code",
        // and this port rebrands the product throughout its user-facing copy.
        // "Claude subscription" is NOT rebranded — that names Anthropic's
        // subscription, not this product.
        assert_eq!(
            OAUTH_ORG_NOT_ALLOWED,
            "Your organization has disabled Claude subscription access for LingXi \u{b7} \
             Use an Anthropic API key instead, or ask your admin to enable access"
        );
    }

    /// The org-disabled copy names two provider-specific things: the env var to
    /// unset, and the account to sign in to. Both must follow the session, not
    /// the oracle's hardcoded Anthropic pair.
    #[test]
    fn org_disabled_names_the_sessions_own_env_var_and_account() {
        // Anthropic stays byte-identical to the oracle (modulo `/connect`).
        assert_eq!(
            api_key_auth_disabled_text(
                &CredentialOrigin::EnvApiKey {
                    var: "ANTHROPIC_API_KEY".to_string()
                },
                false,
                Some("anthropic"),
            ),
            "Your organization has disabled API key authentication \u{b7} Unset \
             ANTHROPIC_API_KEY and run /connect to sign in with your claude.ai account"
        );
        // A session on another provider names ITS variable and ITS account.
        // Telling this user to unset ANTHROPIC_API_KEY would be advice that
        // cannot possibly help them.
        assert_eq!(
            api_key_auth_disabled_text(
                &CredentialOrigin::EnvApiKey {
                    var: "OPENAI_API_KEY".to_string()
                },
                false,
                Some("openai"),
            ),
            "Your organization has disabled API key authentication \u{b7} Unset \
             OPENAI_API_KEY and run /connect to sign in with your OpenAI account"
        );
        // The already-signed-in variant names the var too.
        assert_eq!(
            api_key_auth_disabled_text(
                &CredentialOrigin::EnvApiKey {
                    var: "GEMINI_API_KEY".to_string()
                },
                true,
                Some("gemini"),
            ),
            "Your organization has disabled API key authentication \u{b7} Unset \
             GEMINI_API_KEY to use your Google account instead"
        );
    }

    /// The account NOUN is not the product name: the oracle says "your
    /// claude.ai account", never "your Claude account".
    #[test]
    fn the_account_noun_differs_from_the_product_name() {
        assert_eq!(account_display(Some("anthropic")), "claude.ai");
        assert_eq!(provider_display(Some("anthropic")), "Claude");
        assert_eq!(account_display(Some("openai")), "OpenAI");
        assert_eq!(account_display(Some("gemini")), "Google");
        assert_eq!(account_display(Some("deepseek")), "deepseek");
    }

    /// Copy that names WHOSE account is at fault must name the provider the
    /// session actually talks to. The oracle can hardcode Claude; LingXi cannot.
    #[test]
    fn the_named_product_follows_the_live_provider_profile() {
        assert_eq!(provider_display(Some("anthropic")), "Claude");
        assert_eq!(provider_display(Some("openai")), "OpenAI");
        assert_eq!(provider_display(Some("azure")), "OpenAI");
        assert_eq!(provider_display(Some("gemini")), "Gemini");
        // Vertex is Gemini's profile in `pricing_provider_id_for_profile`;
        // Bedrock hosts Claude. Following that table rather than inventing a
        // second one that can disagree with it.
        assert_eq!(provider_display(Some("vertex")), "Gemini");
        assert_eq!(provider_display(Some("bedrock")), "Claude");
        // An unknown/custom profile names itself rather than guessing a vendor.
        assert_eq!(provider_display(Some("deepseek")), "deepseek");
        // No live profile → the default route.
        assert_eq!(provider_display(None), "Claude");
    }

    /// The non-interactive revoked copy is the one auth string that names a
    /// product AND can be reached by more than one provider: both the Anthropic
    /// and the OpenAI credential providers surface refresh/auth failures into
    /// the same renderer.
    #[test]
    fn the_revoked_copy_names_the_session_provider_not_always_claude() {
        assert_eq!(
            oauth_revoked_text(false, Some("anthropic")),
            "Your account does not have access to Claude. Please login again or \
             contact your administrator."
        );
        assert_eq!(
            oauth_revoked_text(false, Some("openai")),
            "Your account does not have access to OpenAI. Please login again or \
             contact your administrator."
        );
        // The interactive form names a command, not a product, so it does not
        // vary by provider.
        assert_eq!(
            oauth_revoked_text(true, Some("openai")),
            "OAuth token revoked \u{b7} Please run /connect"
        );
    }

    /// Every user-facing auth string that names a command must name
    /// [`AUTH_COMMAND`], and none may reintroduce the oracle's `/login`.
    ///
    /// The multi-provider ruling is easy to undo by accident: a byte-parity
    /// pass diffing against the 2.1.220 binary sees `/connect` as a regression
    /// and "fixes" it. This test is the thing that says no.
    #[test]
    fn every_auth_instruction_names_the_products_own_command() {
        let interactive_copy = [
            NOT_LOGGED_IN.to_string(),
            oauth_revoked_text(true, Some("anthropic")),
            oauth_refresh_dead_text(true).to_string(),
        ];
        for s in &interactive_copy {
            assert!(
                s.contains(AUTH_COMMAND),
                "auth copy must name {AUTH_COMMAND}: {s}"
            );
        }
        // The org-disabled tails that name a command, across every origin.
        let org_disabled = [
            api_key_auth_disabled_text(
                &CredentialOrigin::EnvApiKey {
                    var: "ANTHROPIC_API_KEY".to_string(),
                },
                false,
                Some("anthropic"),
            ),
            api_key_auth_disabled_text(&CredentialOrigin::ApiKeyHelper, true, Some("anthropic")),
            api_key_auth_disabled_text(&CredentialOrigin::LoginManagedKey, true, Some("anthropic")),
        ];
        for s in &org_disabled {
            assert!(
                s.contains(AUTH_COMMAND),
                "org-disabled copy must name {AUTH_COMMAND}: {s}"
            );
        }
        // Nothing user-facing may say `/login` — including the copy that names
        // no command at all.
        for s in interactive_copy.into_iter().chain(org_disabled).chain([
            INVALID_API_KEY.to_string(),
            oauth_revoked_text(false, Some("anthropic")),
            oauth_refresh_dead_text(false).to_string(),
            api_key_auth_disabled_text(
                &CredentialOrigin::EnvApiKey {
                    var: "ANTHROPIC_API_KEY".to_string(),
                },
                true,
                Some("anthropic"),
            ),
        ]) {
            assert!(
                !s.contains("/login"),
                "LingXi has no /login in user-facing copy: {s}"
            );
        }
    }

    #[test]
    fn the_login_expired_copy_splits_on_interactivity() {
        // Oracle `se_` @230618923, verified in the binary in ESCAPED form
        // (`\xB7`) — a literal `·` grep against the binary returns zero.
        // The command is `/connect`, not the oracle's `/login`: deliberate
        // multi-provider divergence, see `AUTH_COMMAND`.
        assert_eq!(
            oauth_refresh_dead_text(true),
            "Login expired \u{b7} Please run /connect"
        );
        // Oracle's `_n()` branch: a non-interactive caller cannot run it,
        // so it states the fact instead of giving an unusable instruction.
        assert_eq!(
            oauth_refresh_dead_text(false),
            "Failed to authenticate: OAuth session expired and could not be refreshed"
        );
    }

    #[test]
    fn the_api_key_header_gate_is_case_insensitive() {
        assert!(mentions_api_key_header("invalid X-Api-Key header"));
        assert!(mentions_api_key_header("401 {\"error\":\"bad x-api-key\"}"));
        assert!(!mentions_api_key_header("authentication_error"));
    }

    #[test]
    fn cloud_credential_copy_splits_on_401() {
        // anthropicAws: 401 means the credential expired; anything else means it
        // is live but cannot reach the model.
        assert_eq!(
            cloud_credential_text(&ErrorRouteTag::AnthropicAws, Some(401)).unwrap(),
            "AWS credentials expired or invalid"
        );
        assert_eq!(
            cloud_credential_text(&ErrorRouteTag::AnthropicAws, Some(403)).unwrap(),
            "AWS authentication failed \u{b7} if credentials are current, check AWS \
             permissions and model access"
        );
        // Bedrock reaches the same branch but never takes the expiry wording:
        // the oracle's inner test is `l==="anthropicAws"||l==="mantle"`.
        assert_eq!(
            cloud_credential_text(
                &ErrorRouteTag::Other {
                    display: "Bedrock".into()
                },
                Some(401)
            )
            .unwrap(),
            "AWS authentication failed \u{b7} if credentials are current, check AWS \
             permissions and model access"
        );
        // GCP always names the re-auth command.
        assert_eq!(
            cloud_credential_text(&ErrorRouteTag::AnthropicGoogleCloud, Some(401)).unwrap(),
            "Google Cloud credentials expired or invalid \u{b7} run \
             `gcloud auth application-default login` and retry"
        );
        assert_eq!(
            cloud_credential_text(&ErrorRouteTag::AnthropicGoogleCloud, Some(500)).unwrap(),
            "Google Cloud authentication failed \u{b7} run \
             `gcloud auth application-default login` and retry"
        );
        // A first-party route is not cloud-hosted → the caller falls through.
        assert!(cloud_credential_text(
            &ErrorRouteTag::FirstParty {
                default_endpoint: true,
                base_url: String::new()
            },
            Some(401)
        )
        .is_none());
    }

    #[test]
    fn the_bare_surfaces_carry_no_prefix() {
        assert_eq!(CREDIT_BALANCE_TOO_LOW, "Credit balance is too low");
        assert_eq!(PROMPT_TOO_LONG, "Prompt is too long");
        for s in [CREDIT_BALANCE_TOO_LOW, PROMPT_TOO_LONG] {
            assert!(!s.starts_with(API_ERROR), "{s} must not be prefixed");
        }
    }

    #[test]
    fn the_rendered_429_is_byte_exact() {
        // The common shape: a decoded body, so the detail comes from JSON.
        let msg =
            r#"429 {"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
        assert_eq!(
            rate_limited_text(msg, REQUEST_REJECTED_429, TEMPORARY_CAPACITY),
            "API Error: Request rejected (429) \u{b7} slow down"
        );
        // The separator is U+00B7, not a hyphen or an ASCII middot lookalike.
        assert!(
            rate_limited_text(msg, REQUEST_REJECTED_429, TEMPORARY_CAPACITY).contains('\u{b7}')
        );
    }

    #[test]
    fn the_first_party_label_swaps_in() {
        assert_eq!(
            rate_limited_text(
                "429 {\"message\":\"x\"}",
                SERVER_LIMITING,
                TEMPORARY_CAPACITY
            ),
            "API Error: Server is temporarily limiting requests (not your usage limit) \u{b7} x"
        );
    }

    #[test]
    fn detail_falls_back_through_json_then_text_then_the_capacity_clause() {
        // `error.message` wins.
        assert_eq!(
            rate_limit_detail(r#"429 {"error":{"message":"a"},"message":"b"}"#),
            "a"
        );
        // Top-level `message` when there is no `error.message`.
        assert_eq!(rate_limit_detail(r#"429 {"message":"b"}"#), "b");
        // Not JSON → the stripped remainder verbatim.
        assert_eq!(
            rate_limit_detail("429 Too Many Requests"),
            "Too Many Requests"
        );
        // Nothing after the status → empty, so the caller's fallback shows.
        assert_eq!(
            rate_limited_text("429 ", REQUEST_REJECTED_429, TEMPORARY_CAPACITY),
            "API Error: Request rejected (429) \u{b7} this may be a temporary capacity issue."
        );
    }

    #[test]
    fn the_status_prefix_strip_requires_whitespace() {
        // `/^429\s+/` — no whitespace, no strip.
        assert_eq!(rate_limit_detail("429Too Many"), "429Too Many");
        // Absent prefix is left alone.
        assert_eq!(rate_limit_detail("Too Many"), "Too Many");
        // Multiple spaces are all consumed (`\s+`).
        assert_eq!(rate_limit_detail("429   spaced"), "spaced");
    }

    #[test]
    fn the_1m_context_copy_is_byte_exact_in_both_modes() {
        assert_eq!(
            usage_credits_required_for_1m_context(false),
            "API Error: Usage credits required for 1M context \u{b7} run /usage-credits to \
             turn them on, or /model to switch to standard context"
        );
        assert_eq!(
            usage_credits_required_for_1m_context(true),
            "API Error: Usage credits required for 1M context \u{b7} turn on usage credits at \
             claude.ai/settings/usage?from=cc_cli_limit_message, or use --model to switch to \
             standard context"
        );
    }

    #[test]
    fn the_long_context_credit_gate_matches_both_phrasings() {
        assert!(is_long_context_credit_message(
            "429 Extra usage is required for long context requests"
        ));
        assert!(is_long_context_credit_message(
            "Usage credits are required for long context"
        ));
        assert!(!is_long_context_credit_message("rate limited"));
    }
}
