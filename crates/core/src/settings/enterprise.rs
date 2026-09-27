//! Enterprise login / version managed-policy keys (parity 2.1.207 H-BIN-09).
//!
//! Six admin-provisioned `managed-settings.json` keys that CC 2.1.207 both
//! *schemas* and *actively enforces* — none of which had a schema entry or a
//! consumer in lingxi before this module:
//!
//! | key | CC role | lingxi wiring |
//! |---|---|---|
//! | `requiredMinimumVersion` | startup exit gate (`a1p`/`c1p`) | **LIVE** — [`version_gate`], wired at CLI boot |
//! | `requiredMaximumVersion` | startup exit gate (same fn) | **LIVE** — [`version_gate`] |
//! | `forceLoginMethod` | pre-select+lock the OAuth login method; non-interactive `gateway` lockout | helpers here; login-flow wiring DORMANT (see below) |
//! | `forceLoginGatewayUrl` | pre-fill the Cloud gateway URL in the login screen | accessor here; login-flow wiring DORMANT |
//! | `forceLoginOrgUUID` | pin OAuth login to an org (or list) | **LIVE** — [`parse_force_login_org_uuid`] + [`check_org_membership`], enforced in the interactive Anthropic OAuth login (`EngineOAuthConnect`) |
//! | `parentSettingsBehavior` | whether the SDK `--managed-settings` parent tier merges under the admin tier | [`should_merge_parent_settings`] predicate; the SDK parent tier itself does not exist in lingxi |
//! | `forceRemoteSettingsRefresh` | block startup until remote managed settings are re-fetched | schema-only; lingxi has no remote-settings fetcher |
//! | `minimumVersion` | USER setting: auto-updater channel-downgrade guard | schema-only; lingxi has no auto-updater |
//!
//! DORMANT surfaces (subsystem genuinely absent in lingxi, faithful CONFIG
//! surface landed so managed settings carrying these keys are ACCESSIBLE and
//! MERGED rather than silently dropped):
//! - `forceLoginOrgUUID` is now LIVE: the interactive Anthropic OAuth login
//!   (`EngineOAuthConnect::login("anthropic")`) reads the folded pin
//!   ([`fold_force_login_org_pin`]) and runs [`check_org_membership`] against the
//!   authenticated account's resolved org, failing + rolling back a forbidden
//!   login with the byte-exact admin messages. CC's `auth_force_login_org`
//!   telemetry is NOT emitted: its four reasons (`managed_by_host_under_pin`,
//!   `unix_socket_{3p,ssh}_under_pin`, `unix_socket_unreadable_policy`) all name
//!   the SDK-host / unix-socket credential-broker contexts lingxi lacks — CC
//!   itself emits no such event on the interactive OAuth denial path, so
//!   emitting one here would fabricate a context.
//! - the interactive method-lock (`forceLoginMethod` pre-select) + gateway-URL
//!   pre-fill still live only as the pure helpers here (method resolution,
//!   pre-select messages, gateway lockout message); wiring them into the TUI
//!   login picker requires threading resolved policy through the app mount and
//!   is deferred.
//! - `minimumVersion`'s sole CC consumer is the auto-updater channel-downgrade
//!   guard (`Zlo`/`Wn()`); lingxi has no auto-updater, so it is dead-by-missing
//!   -subsystem — schema + accessor only, do NOT invent an updater.
//! - `forceRemoteSettingsRefresh` gates a remote managed-settings fetch lingxi
//!   does not perform — schema only.
//!
//! Oracle: CC 2.1.207 binary. Version-gate strings/logic verbatim from `a1p`
//! (`function a1p({currentVersion:e,requiredMinimumVersion:t,requiredMaximumVersion:r,topLevelCommand:n})`);
//! login strings verbatim from the `$Xg`/getSettingsForSource projection.
//! Product-facing prose ("Claude Code") is rebranded to `branding::PRODUCT_NAME`
//! and the invocation form (`claude update` → `lingxi-cli update`) per repo
//! rebrand precedent (`apps/cli/src/commands/install.rs`).

use crate::settings::schema::SettingsJson;

// ── Startup version gate (requiredMinimumVersion / requiredMaximumVersion) ───

/// Top-level commands EXEMPT from the managed startup version gate — verbatim
/// CC `aW_=new Set(["update","install","doctor"])`. A user pinned below the
/// org minimum must still be able to `update`/`install`/`doctor` their way out.
pub const EXEMPT_TOP_LEVEL_COMMANDS: &[&str] = &["update", "install", "doctor"];

/// A parsed semver core + prerelease (build metadata ignored for precedence),
/// mirroring npm `semver.parse` returning `null` on non-semver input (CC
/// `DMo.parse`). Only strict `MAJOR.MINOR.PATCH[-prerelease][+build]` parses.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    /// Dot-separated prerelease identifiers (empty ⇒ no prerelease).
    prerelease: Vec<String>,
}

/// Parse a strict semver string, or `None` if it is not valid semver (CC
/// `DMo.parse(x)` ⇒ `null`). Requires exactly three numeric core components;
/// an optional `-prerelease` (dot-separated identifiers) and `+build` (ignored)
/// may follow. Non-numeric core, missing components, or an empty prerelease
/// identifier reject.
fn parse_semver(input: &str) -> Option<SemVer> {
    // Strip build metadata (everything from the first '+').
    let no_build = input.split('+').next().unwrap_or(input);
    // Separate core from prerelease at the first '-'.
    let (core, pre) = match no_build.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (no_build, None),
    };
    let mut parts = core.split('.');
    let major = parse_numeric_id(parts.next()?)?;
    let minor = parse_numeric_id(parts.next()?)?;
    let patch = parse_numeric_id(parts.next()?)?;
    if parts.next().is_some() {
        return None; // more than three core components
    }
    let prerelease = match pre {
        None => Vec::new(),
        Some(p) => {
            let ids: Vec<String> = p.split('.').map(str::to_string).collect();
            // No identifier may be empty; alphanumeric identifiers are limited
            // to [0-9A-Za-z-] (npm `semver` rejects others).
            for id in &ids {
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                    return None;
                }
            }
            ids
        }
    };
    Some(SemVer {
        major,
        minor,
        patch,
        prerelease,
    })
}

/// Parse a numeric core identifier: all-ASCII-digits, no leading zero unless the
/// value is exactly `"0"` (strict semver).
fn parse_numeric_id(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() > 1 && s.starts_with('0') {
        return None; // leading zero
    }
    s.parse::<u64>().ok()
}

/// Semver precedence compare (CC `aUn` = `semver.compare`): core compared
/// numerically; then a version WITH a prerelease sorts below the same core
/// WITHOUT one; prerelease identifiers compared left-to-right (numeric <
/// alphanumeric; numeric numerically; alphanumeric by ASCII; a longer set wins
/// when all preceding identifiers are equal).
fn compare_semver(a: &SemVer, b: &SemVer) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let core = a
        .major
        .cmp(&b.major)
        .then(a.minor.cmp(&b.minor))
        .then(a.patch.cmp(&b.patch));
    if core != Ordering::Equal {
        return core;
    }
    match (a.prerelease.is_empty(), b.prerelease.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater, // no-prerelease > has-prerelease
        (false, true) => Ordering::Less,
        (false, false) => compare_prerelease(&a.prerelease, &b.prerelease),
    }
}

fn compare_prerelease(a: &[String], b: &[String]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (ia, ib) in a.iter().zip(b.iter()) {
        let na = ia.parse::<u64>().ok();
        let nb = ib.parse::<u64>().ok();
        let ord = match (na, nb) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Less, // numeric < alphanumeric
            (None, Some(_)) => Ordering::Greater,
            (None, None) => ia.as_bytes().cmp(ib.as_bytes()),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

/// The managed (`policySettings`) version-policy view consumed by the startup
/// gate — CC reads these off `wr("policySettings")` (the merged managed
/// settings) in `c1p`. Scalar keys, so the highest-priority managed tier wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManagedVersionPolicy {
    /// `requiredMinimumVersion` — running below it exits at startup.
    pub required_minimum_version: Option<String>,
    /// `requiredMaximumVersion` — running above it exits at startup.
    pub required_maximum_version: Option<String>,
}

/// Fold the raw managed-settings tiers (ASCENDING priority, as returned by
/// `settings_watch::managed_settings_raw_tiers`) into the version policy the
/// startup gate reads. Last tier wins per scalar key; a tier that fails to
/// parse is skipped (the gate is fail-OPEN on an unreadable policy — matching
/// CC's `c1p` `try{…}catch(e){return Re(e),null}` which swallows the read
/// error and returns "no gate").
#[must_use]
pub fn managed_version_policy(raw_tiers: &[String]) -> ManagedVersionPolicy {
    let mut out = ManagedVersionPolicy::default();
    for raw in raw_tiers {
        if let Ok(s) = serde_json::from_str::<SettingsJson>(raw) {
            if s.required_minimum_version.is_some() {
                out.required_minimum_version = s.required_minimum_version;
            }
            if s.required_maximum_version.is_some() {
                out.required_maximum_version = s.required_maximum_version;
            }
        }
    }
    out
}

/// The managed startup version gate — a faithful port of CC `a1p`.
///
/// Returns `Some(message)` when the running `current_version` violates an
/// org-set `required_minimum_version` / `required_maximum_version` bound (the
/// caller prints it to stderr and exits 1); `None` when the run may proceed.
///
/// `top_level_command` is the resolved subcommand name (`None` for a bare
/// session); `update`/`install`/`doctor` are exempt. `log` receives the
/// error-level "not a valid semver version — ignoring" diagnostics for a
/// malformed bound (CC logs these at `level:"error"` and proceeds).
///
/// Byte-for-byte CC logic:
/// - `if(!t&&!r)return null` — no gate when neither bound is set.
/// - `if(n!==void 0&&aW_.has(n))return null` — exempt commands.
/// - `if(!DMo.parse(e))return null` — un-parseable current version ⇒ no gate.
/// - min: `if(!OO(e,o))` (current `<` min) ⇒ exit; malformed min ⇒ log+ignore.
/// - max: `if(!vjt(e,o))` (current `>` max) ⇒ exit; malformed max ⇒ log+ignore.
#[must_use]
pub fn version_gate(
    current_version: &str,
    required_minimum_version: Option<&str>,
    required_maximum_version: Option<&str>,
    top_level_command: Option<&str>,
    log: &mut dyn FnMut(&str),
) -> Option<String> {
    use std::cmp::Ordering;
    // `if(!t&&!r)return null`
    if required_minimum_version.is_none() && required_maximum_version.is_none() {
        return None;
    }
    // `if(n!==void 0&&aW_.has(n))return null`
    if let Some(cmd) = top_level_command {
        if EXEMPT_TOP_LEVEL_COMMANDS.contains(&cmd) {
            return None;
        }
    }
    // `if(!DMo.parse(e))return null`
    let cur = parse_semver(current_version)?;
    let product = branding::PRODUCT_NAME;

    if let Some(min) = required_minimum_version {
        match parse_semver(min) {
            None => log(&format!(
                "requiredMinimumVersion '{min}' is not a valid semver version — ignoring"
            )),
            Some(minv) => {
                // `if(!OO(e,o))` ⇒ current < min.
                if compare_semver(&cur, &minv) == Ordering::Less {
                    return Some(format!(
                        "{product} {current_version} is older than the minimum version required by your organization ({min}).\n\
Update {product} using your organization's approved method, then try again. If automatic updates are available, `lingxi-cli update` may also work."
                    ));
                }
            }
        }
    }

    if let Some(max) = required_maximum_version {
        match parse_semver(max) {
            None => log(&format!(
                "requiredMaximumVersion '{max}' is not a valid semver version — ignoring"
            )),
            Some(maxv) => {
                // `if(!vjt(e,o))` ⇒ current > max.
                if compare_semver(&cur, &maxv) == Ordering::Greater {
                    return Some(format!(
                        "{product} {current_version} is newer than the maximum version allowed by your organization ({max}).\n\
Your organization requires version {max} or older. Install an approved version using your organization's approved method. `lingxi-cli install <version>` may also work."
                    ));
                }
            }
        }
    }

    None
}

// ── parentSettingsBehavior ───────────────────────────────────────────────────

/// CC `USm(e){return !e || e.parentSettingsBehavior==="merge"}` — whether the
/// SDK parent managed-settings tier (`Options.managedSettings` /
/// `--managed-settings`) layers UNDER the admin tier.
///
/// `admin_parent_settings_behavior` is the admin tier's `parentSettingsBehavior`
/// value (`None` when there is no admin tier at all). Default (`"first-wins"`
/// or absent-on-a-present-admin-tier) drops the parent; only `"merge"` keeps it.
///
/// NOTE: the SDK parent tier itself (`--managed-settings`) does not exist in
/// lingxi yet, so this predicate has no live parent input to gate — it is
/// landed as the faithful merge predicate + schema so the machinery is ready
/// when that tier is added. CC's companion `qSm(parent, admin)` restrictive-only
/// filter is the parent-slice builder that would consume this; it is intentionally
/// NOT ported here (no parent slice to filter).
#[must_use]
pub fn should_merge_parent_settings(admin_parent_settings_behavior: Option<&str>) -> bool {
    match admin_parent_settings_behavior {
        None => true,
        Some(b) => b == "merge",
    }
}

// ── forceLoginMethod ─────────────────────────────────────────────────────────

/// The three admin-forceable OAuth login methods (CC
/// `forceLoginMethod:E.enum(["claudeai","console","gateway"])`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceLoginMethod {
    /// Claude Pro/Max subscription (`"claudeai"`).
    ClaudeAi,
    /// Anthropic Console API billing (`"console"`).
    Console,
    /// Cloud gateway OIDC device flow (`"gateway"`).
    Gateway,
}

impl ForceLoginMethod {
    /// The wire enum literal.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            ForceLoginMethod::ClaudeAi => "claudeai",
            ForceLoginMethod::Console => "console",
            ForceLoginMethod::Gateway => "gateway",
        }
    }

    /// The login-picker pre-selection banner CC shows when the method is locked
    /// by policy (verbatim: `"Login method pre-selected: …"`). `gateway` has no
    /// banner (CC's `y` ternary yields `null` for it).
    #[must_use]
    pub fn preselect_message(self) -> Option<&'static str> {
        match self {
            ForceLoginMethod::ClaudeAi => {
                Some("Login method pre-selected: Subscription Plan (Claude Pro/Max)")
            }
            ForceLoginMethod::Console => {
                Some("Login method pre-selected: API usage billing (Anthropic Console)")
            }
            ForceLoginMethod::Gateway => None,
        }
    }
}

/// Parse `forceLoginMethod`, degrading an unknown value to `None` — faithful to
/// the zod `.catch(void 0)` on the enum (an out-of-set value becomes
/// `undefined`, i.e. "no forced method").
#[must_use]
pub fn parse_force_login_method(value: &str) -> Option<ForceLoginMethod> {
    match value {
        "claudeai" => Some(ForceLoginMethod::ClaudeAi),
        "console" => Some(ForceLoginMethod::Console),
        "gateway" => Some(ForceLoginMethod::Gateway),
        _ => None,
    }
}

/// CC's non-interactive gateway lockout message (verbatim). Emitted to stderr
/// with `process.exit(1)` when a non-interactive login is attempted while
/// managed `forceLoginMethod === "gateway"` (`$Xg`:
/// `if(u4t(sBe())&&i?.forceLoginMethod==="gateway")…`). The interactive
/// `/login` flow instead pre-selects the gateway method.
pub const GATEWAY_NONINTERACTIVE_LOCKOUT: &str =
    "forceLoginMethod is 'gateway' in managed settings; run interactive /login to authenticate.";

// ── forceLoginOrgUUID ────────────────────────────────────────────────────────

/// Validation warning emitted when `forceLoginOrgUUID` is present but not a
/// string / array-of-strings (CC getSettingsForSource projection `.catch`:
/// degrade to "no org permitted"). Verbatim.
pub const FORCE_LOGIN_ORG_UUID_INVALID: &str =
    "\"forceLoginOrgUUID\" was present but invalid; no organization is permitted to log in until it is fixed.";

/// Admin-error message when `forceLoginOrgUUID` is an explicit empty array
/// (CC membership check: `if(n.length===0)return{valid:!1,message:…}`). Verbatim.
pub const FORCE_LOGIN_ORG_UUID_EMPTY_ARRAY: &str = "forceLoginOrgUUID in managed settings is set to an empty array.\nNo organizations are permitted. This is almost certainly a misconfiguration.\nContact your administrator.";

/// Parsed `forceLoginOrgUUID` — the org pin applied to OAuth login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForceLoginOrgPin {
    /// Key absent ⇒ no organization pin (login unrestricted).
    Unset,
    /// A non-empty list of permitted org UUIDs (a single string normalizes to a
    /// one-element list; any one match permits login).
    Pinned(Vec<String>),
    /// Present but the schema union failed (not a string / array-of-strings, or
    /// an array containing a non-string). No org permitted until fixed;
    /// carries [`FORCE_LOGIN_ORG_UUID_INVALID`].
    Invalid,
    /// Explicit empty array — admin misconfiguration; no org permitted; carries
    /// [`FORCE_LOGIN_ORG_UUID_EMPTY_ARRAY`].
    EmptyArray,
}

/// The outcome of checking an authenticated account's org memberships against
/// the pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrgMembershipCheck {
    /// Login permitted (no pin, or the account belongs to a permitted org).
    Permitted,
    /// Login denied; carries the byte-exact admin/validation message to surface.
    Denied(String),
}

/// Parse the raw `forceLoginOrgUUID` settings value into a [`ForceLoginOrgPin`].
/// `None` ⇒ [`ForceLoginOrgPin::Unset`]. A string ⇒ single-element pin; a
/// non-empty array of strings ⇒ that pin; `[]` ⇒ [`ForceLoginOrgPin::EmptyArray`];
/// any other shape ⇒ [`ForceLoginOrgPin::Invalid`] (the zod union catch).
#[must_use]
pub fn parse_force_login_org_uuid(value: Option<&serde_json::Value>) -> ForceLoginOrgPin {
    use serde_json::Value;
    match value {
        None | Some(Value::Null) => ForceLoginOrgPin::Unset,
        Some(Value::String(s)) => ForceLoginOrgPin::Pinned(vec![s.clone()]),
        Some(Value::Array(arr)) => {
            if arr.is_empty() {
                return ForceLoginOrgPin::EmptyArray;
            }
            let mut out = Vec::with_capacity(arr.len());
            for v in arr {
                match v.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => return ForceLoginOrgPin::Invalid, // non-string element
                }
            }
            ForceLoginOrgPin::Pinned(out)
        }
        Some(_) => ForceLoginOrgPin::Invalid,
    }
}

/// The required-organization clause of the interactive-OAuth mismatch message.
/// A single pinned org reads `organization {uuid}`; multiple read
/// `one of these organizations: {a, b, …}` (CC join `", "`). Verbatim CC pieces
/// (`organization `, `one of these organizations: `, `, `). A single-element
/// `Pinned` (whether the admin wrote a bare string or a one-item array) takes the
/// singular form, matching CC's `.length === 1` branch on the normalized array.
#[must_use]
fn force_login_required_description(permitted: &[String]) -> String {
    if permitted.len() == 1 {
        format!("organization {}", permitted[0])
    } else {
        format!("one of these organizations: {}", permitted.join(", "))
    }
}

/// The byte-exact interactive-OAuth org-mismatch denial (CC `$Xg` assembly):
/// `Your authentication token belongs to organization {token_org},\nbut this
/// machine requires {required}.\n\nPlease log in with a permitted organization:
/// {login_cmd}`. Verbatim CC except the `claude auth login` invocation, which is
/// rebranded to `lingxi-cli auth login` per repo precedent (the same rebrand the
/// version gate applies to `claude update`, and `apps/cli/.../auth.rs`'s
/// not-logged-in line).
#[must_use]
fn org_mismatch_message(token_org: &str, permitted: &[String]) -> String {
    format!(
        "Your authentication token belongs to organization {token_org},\n\
but this machine requires {}.\n\n\
Please log in with a permitted organization: lingxi-cli auth login",
        force_login_required_description(permitted)
    )
}

/// Check an authenticated account's org memberships against the pin, returning
/// the byte-exact denial message when login is forbidden. `account_org_ids` is
/// the set of org UUIDs the authenticated account belongs to (lingxi's
/// first-party OAuth resolves exactly one — the account's `organization.uuid` —
/// so this is a one-element slice in practice).
#[must_use]
pub fn check_org_membership(
    pin: &ForceLoginOrgPin,
    account_org_ids: &[String],
) -> OrgMembershipCheck {
    match pin {
        ForceLoginOrgPin::Unset => OrgMembershipCheck::Permitted,
        ForceLoginOrgPin::Invalid => {
            OrgMembershipCheck::Denied(FORCE_LOGIN_ORG_UUID_INVALID.to_string())
        }
        ForceLoginOrgPin::EmptyArray => {
            OrgMembershipCheck::Denied(FORCE_LOGIN_ORG_UUID_EMPTY_ARRAY.to_string())
        }
        ForceLoginOrgPin::Pinned(permitted) => {
            if account_org_ids.iter().any(|o| permitted.contains(o)) {
                OrgMembershipCheck::Permitted
            } else {
                // CC's interactive-OAuth org-mismatch block: "Your authentication
                // token belongs to organization {tokenOrg}, but this machine
                // requires {required}." The displayed token org is the account's
                // resolved org (the sole element for lingxi's first-party login).
                let token_org = account_org_ids.first().map(String::as_str).unwrap_or("");
                OrgMembershipCheck::Denied(org_mismatch_message(token_org, permitted))
            }
        }
    }
}

/// Fold the raw managed-settings tiers (ASCENDING priority, as returned by
/// `settings_watch::managed_settings_raw_tiers`) into the effective
/// `forceLoginOrgUUID` pin. `forceLoginOrgUUID` is a scalar key, so the
/// highest-priority tier that SETS it wins (later source overrides). A tier that
/// fails to parse is skipped (fail-OPEN, matching [`managed_version_policy`] and
/// CC's `try{…}catch` swallow of an unreadable interactive-login policy; the
/// fail-CLOSED unreadable-policy path is a non-interactive credential-broker
/// context — `unix_socket_unreadable_policy` — that lingxi does not have).
#[must_use]
pub fn fold_force_login_org_pin(raw_tiers: &[String]) -> ForceLoginOrgPin {
    let mut pin = ForceLoginOrgPin::Unset;
    for raw in raw_tiers {
        if let Ok(s) = serde_json::from_str::<SettingsJson>(raw) {
            if s.force_login_org_uuid.is_some() {
                pin = s.force_login_org_pin();
            }
        }
    }
    pin
}

// ── SettingsJson typed accessors ─────────────────────────────────────────────

impl SettingsJson {
    /// Typed `forceLoginMethod` (out-of-set value ⇒ `None`, per zod `.catch`).
    #[must_use]
    pub fn force_login_method_parsed(&self) -> Option<ForceLoginMethod> {
        self.force_login_method
            .as_deref()
            .and_then(parse_force_login_method)
    }

    /// Typed `forceLoginOrgUUID` org pin.
    #[must_use]
    pub fn force_login_org_pin(&self) -> ForceLoginOrgPin {
        parse_force_login_org_uuid(self.force_login_org_uuid.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── version gate ─────────────────────────────────────────────────────────

    fn no_log() -> impl FnMut(&str) {
        |_: &str| {}
    }

    #[test]
    fn version_gate_no_bounds_is_noop() {
        let mut log = no_log();
        assert_eq!(version_gate("2.1.207", None, None, None, &mut log), None);
    }

    #[test]
    fn version_gate_below_minimum_exits_byte_exact() {
        let mut log = no_log();
        let msg = version_gate("2.1.100", Some("2.1.207"), None, None, &mut log)
            .expect("below minimum must gate");
        assert_eq!(
            msg,
            "LingXi 2.1.100 is older than the minimum version required by your organization (2.1.207).\n\
Update LingXi using your organization's approved method, then try again. If automatic updates are available, `lingxi-cli update` may also work."
        );
    }

    #[test]
    fn version_gate_at_or_above_minimum_passes() {
        let mut log = no_log();
        // Exactly the minimum ⇒ OK (CC `OO` is `>=`).
        assert_eq!(
            version_gate("2.1.207", Some("2.1.207"), None, None, &mut log),
            None
        );
        assert_eq!(
            version_gate("2.2.0", Some("2.1.207"), None, None, &mut log),
            None
        );
    }

    #[test]
    fn version_gate_above_maximum_exits_byte_exact() {
        let mut log = no_log();
        let msg = version_gate("3.0.0", None, Some("2.9.0"), None, &mut log)
            .expect("above maximum must gate");
        assert_eq!(
            msg,
            "LingXi 3.0.0 is newer than the maximum version allowed by your organization (2.9.0).\n\
Your organization requires version 2.9.0 or older. Install an approved version using your organization's approved method. `lingxi-cli install <version>` may also work."
        );
    }

    #[test]
    fn version_gate_at_or_below_maximum_passes() {
        let mut log = no_log();
        assert_eq!(
            version_gate("2.9.0", None, Some("2.9.0"), None, &mut log),
            None
        );
        assert_eq!(
            version_gate("2.8.0", None, Some("2.9.0"), None, &mut log),
            None
        );
    }

    #[test]
    fn version_gate_exempts_update_install_doctor() {
        let mut log = no_log();
        for cmd in ["update", "install", "doctor"] {
            assert_eq!(
                version_gate("1.0.0", Some("2.1.207"), None, Some(cmd), &mut log),
                None,
                "{cmd} must be exempt from the version gate"
            );
        }
        // A non-exempt command is still gated.
        assert!(version_gate("1.0.0", Some("2.1.207"), None, Some("mcp"), &mut log).is_some());
        // A bare session (no command) is gated.
        assert!(version_gate("1.0.0", Some("2.1.207"), None, None, &mut log).is_some());
    }

    #[test]
    fn version_gate_invalid_min_logs_and_ignores() {
        let mut warnings: Vec<String> = Vec::new();
        let out = version_gate("2.1.207", Some("latest"), None, None, &mut |m| {
            warnings.push(m.to_string())
        });
        assert_eq!(out, None, "an un-parseable minimum must not gate");
        assert_eq!(
            warnings,
            vec![
                "requiredMinimumVersion 'latest' is not a valid semver version — ignoring"
                    .to_string()
            ]
        );
    }

    #[test]
    fn version_gate_invalid_max_logs_and_ignores() {
        let mut warnings: Vec<String> = Vec::new();
        let out = version_gate("2.1.207", None, Some("2.x"), None, &mut |m| {
            warnings.push(m.to_string())
        });
        assert_eq!(out, None);
        assert_eq!(
            warnings,
            vec![
                "requiredMaximumVersion '2.x' is not a valid semver version — ignoring".to_string()
            ]
        );
    }

    #[test]
    fn version_gate_unparseable_current_version_is_noop() {
        // CC `if(!DMo.parse(e))return null` — a non-semver current version
        // disables the gate entirely.
        let mut log = no_log();
        assert_eq!(
            version_gate("dev-build", Some("2.1.207"), None, None, &mut log),
            None
        );
    }

    #[test]
    fn version_gate_prerelease_precedence() {
        let mut log = no_log();
        // 2.1.0-beta < 2.1.0 (prerelease sorts below release) ⇒ below min.
        assert!(version_gate("2.1.0-beta", Some("2.1.0"), None, None, &mut log).is_some());
        // 2.1.0 >= 2.1.0-beta ⇒ passes a min of 2.1.0-beta.
        assert_eq!(
            version_gate("2.1.0", Some("2.1.0-beta"), None, None, &mut log),
            None
        );
    }

    #[test]
    fn semver_parse_rejects_non_semver() {
        assert!(parse_semver("latest").is_none());
        assert!(parse_semver("2.1").is_none());
        assert!(parse_semver("2").is_none());
        assert!(parse_semver("2.1.x").is_none());
        assert!(parse_semver("2.1.07").is_none()); // leading zero
        assert!(parse_semver("").is_none());
        assert!(parse_semver("2.1.0-").is_none()); // empty prerelease id
        assert_eq!(
            parse_semver("2.1.207"),
            Some(SemVer {
                major: 2,
                minor: 1,
                patch: 207,
                prerelease: vec![]
            })
        );
        // Build metadata is ignored for parse success.
        assert!(parse_semver("2.1.0+build.5").is_some());
    }

    #[test]
    fn semver_compare_numeric_and_prerelease() {
        use std::cmp::Ordering;
        let a = parse_semver("2.1.100").unwrap();
        let b = parse_semver("2.1.99").unwrap();
        assert_eq!(compare_semver(&a, &b), Ordering::Greater); // 100 > 99 numerically
        let pre = parse_semver("1.0.0-alpha").unwrap();
        let rel = parse_semver("1.0.0").unwrap();
        assert_eq!(compare_semver(&pre, &rel), Ordering::Less);
        let a1 = parse_semver("1.0.0-alpha.1").unwrap();
        let a2 = parse_semver("1.0.0-alpha.2").unwrap();
        assert_eq!(compare_semver(&a1, &a2), Ordering::Less);
    }

    // ── managed_version_policy fold ──────────────────────────────────────────

    #[test]
    fn managed_version_policy_last_tier_wins() {
        let tiers = vec![
            r#"{"requiredMinimumVersion":"2.0.0"}"#.to_string(),
            r#"{"requiredMinimumVersion":"2.1.207","requiredMaximumVersion":"3.0.0"}"#.to_string(),
        ];
        let p = managed_version_policy(&tiers);
        assert_eq!(p.required_minimum_version.as_deref(), Some("2.1.207"));
        assert_eq!(p.required_maximum_version.as_deref(), Some("3.0.0"));
    }

    #[test]
    fn managed_version_policy_skips_unparseable_tier() {
        let tiers = vec![
            r#"{"requiredMinimumVersion":"2.1.0"}"#.to_string(),
            r#"{not valid json"#.to_string(),
        ];
        let p = managed_version_policy(&tiers);
        assert_eq!(p.required_minimum_version.as_deref(), Some("2.1.0"));
    }

    #[test]
    fn managed_version_policy_empty_is_default() {
        assert_eq!(managed_version_policy(&[]), ManagedVersionPolicy::default());
    }

    // ── parentSettingsBehavior ───────────────────────────────────────────────

    #[test]
    fn parent_settings_behavior_predicate() {
        // No admin tier ⇒ parent merges (applies as sole policy tier).
        assert!(should_merge_parent_settings(None));
        // Explicit "merge" ⇒ merge.
        assert!(should_merge_parent_settings(Some("merge")));
        // Default "first-wins" (or any other value) ⇒ drop the parent.
        assert!(!should_merge_parent_settings(Some("first-wins")));
        assert!(!should_merge_parent_settings(Some("bogus")));
    }

    // ── forceLoginMethod ─────────────────────────────────────────────────────

    #[test]
    fn force_login_method_parse_and_catch() {
        assert_eq!(
            parse_force_login_method("claudeai"),
            Some(ForceLoginMethod::ClaudeAi)
        );
        assert_eq!(
            parse_force_login_method("console"),
            Some(ForceLoginMethod::Console)
        );
        assert_eq!(
            parse_force_login_method("gateway"),
            Some(ForceLoginMethod::Gateway)
        );
        // zod `.catch(void 0)` — an out-of-set value degrades to None.
        assert_eq!(parse_force_login_method("sso"), None);
        assert_eq!(parse_force_login_method(""), None);
    }

    #[test]
    fn force_login_method_preselect_messages() {
        assert_eq!(
            ForceLoginMethod::ClaudeAi.preselect_message(),
            Some("Login method pre-selected: Subscription Plan (Claude Pro/Max)")
        );
        assert_eq!(
            ForceLoginMethod::Console.preselect_message(),
            Some("Login method pre-selected: API usage billing (Anthropic Console)")
        );
        // gateway has no pre-select banner (CC's `y` ternary ⇒ null).
        assert_eq!(ForceLoginMethod::Gateway.preselect_message(), None);
    }

    #[test]
    fn gateway_lockout_message_is_byte_exact() {
        assert_eq!(
            GATEWAY_NONINTERACTIVE_LOCKOUT,
            "forceLoginMethod is 'gateway' in managed settings; run interactive /login to authenticate."
        );
    }

    // ── forceLoginOrgUUID ────────────────────────────────────────────────────

    #[test]
    fn force_login_org_uuid_parses_all_shapes() {
        use serde_json::json;
        assert_eq!(parse_force_login_org_uuid(None), ForceLoginOrgPin::Unset);
        assert_eq!(
            parse_force_login_org_uuid(Some(&json!("org-1"))),
            ForceLoginOrgPin::Pinned(vec!["org-1".to_string()])
        );
        assert_eq!(
            parse_force_login_org_uuid(Some(&json!(["org-1", "org-2"]))),
            ForceLoginOrgPin::Pinned(vec!["org-1".to_string(), "org-2".to_string()])
        );
        assert_eq!(
            parse_force_login_org_uuid(Some(&json!([]))),
            ForceLoginOrgPin::EmptyArray
        );
        // Non-string element, or wrong type ⇒ Invalid (zod union catch).
        assert_eq!(
            parse_force_login_org_uuid(Some(&json!(["org-1", 5]))),
            ForceLoginOrgPin::Invalid
        );
        assert_eq!(
            parse_force_login_org_uuid(Some(&json!(42))),
            ForceLoginOrgPin::Invalid
        );
    }

    #[test]
    fn org_membership_check_messages_byte_exact() {
        // Unset ⇒ permitted.
        assert_eq!(
            check_org_membership(&ForceLoginOrgPin::Unset, &[]),
            OrgMembershipCheck::Permitted
        );
        // Empty array ⇒ denied with the byte-exact admin message.
        assert_eq!(
            check_org_membership(&ForceLoginOrgPin::EmptyArray, &["org-1".to_string()]),
            OrgMembershipCheck::Denied(
                "forceLoginOrgUUID in managed settings is set to an empty array.\nNo organizations are permitted. This is almost certainly a misconfiguration.\nContact your administrator.".to_string()
            )
        );
        // Invalid ⇒ denied with the byte-exact validation message.
        assert_eq!(
            check_org_membership(&ForceLoginOrgPin::Invalid, &["org-1".to_string()]),
            OrgMembershipCheck::Denied(
                "\"forceLoginOrgUUID\" was present but invalid; no organization is permitted to log in until it is fixed.".to_string()
            )
        );
        // Pinned + member ⇒ permitted (any one match).
        assert_eq!(
            check_org_membership(
                &ForceLoginOrgPin::Pinned(vec!["org-a".to_string(), "org-b".to_string()]),
                &["org-x".to_string(), "org-b".to_string()]
            ),
            OrgMembershipCheck::Permitted
        );
    }

    #[test]
    fn org_mismatch_message_single_pin_byte_exact() {
        // Single pinned org ⇒ the singular "organization {uuid}" clause; the
        // displayed token org is the account's resolved org.
        let denied = check_org_membership(
            &ForceLoginOrgPin::Pinned(vec!["org-required".to_string()]),
            &["org-token".to_string()],
        );
        assert_eq!(
            denied,
            OrgMembershipCheck::Denied(
                "Your authentication token belongs to organization org-token,\n\
but this machine requires organization org-required.\n\n\
Please log in with a permitted organization: lingxi-cli auth login"
                    .to_string()
            )
        );
    }

    #[test]
    fn org_mismatch_message_array_pin_byte_exact() {
        // Multiple pinned orgs ⇒ the plural "one of these organizations: …"
        // clause, comma-space joined in pin order.
        let denied = check_org_membership(
            &ForceLoginOrgPin::Pinned(vec!["org-a".to_string(), "org-b".to_string()]),
            &["org-token".to_string()],
        );
        assert_eq!(
            denied,
            OrgMembershipCheck::Denied(
                "Your authentication token belongs to organization org-token,\n\
but this machine requires one of these organizations: org-a, org-b.\n\n\
Please log in with a permitted organization: lingxi-cli auth login"
                    .to_string()
            )
        );
    }

    #[test]
    fn fold_force_login_org_pin_last_setting_tier_wins() {
        // Absent in every tier ⇒ Unset.
        assert_eq!(
            fold_force_login_org_pin(&[r#"{}"#.to_string()]),
            ForceLoginOrgPin::Unset
        );
        // Highest-priority tier that SETS the key wins (scalar override).
        let tiers = vec![
            r#"{"forceLoginOrgUUID":"org-base"}"#.to_string(),
            r#"{"forceLoginOrgUUID":["org-x","org-y"]}"#.to_string(),
        ];
        assert_eq!(
            fold_force_login_org_pin(&tiers),
            ForceLoginOrgPin::Pinned(vec!["org-x".to_string(), "org-y".to_string()])
        );
        // A tier absent the key does not clear a lower tier's pin.
        let tiers2 = vec![
            r#"{"forceLoginOrgUUID":"org-keep"}"#.to_string(),
            r#"{"defaultMode":"acceptEdits"}"#.to_string(),
        ];
        assert_eq!(
            fold_force_login_org_pin(&tiers2),
            ForceLoginOrgPin::Pinned(vec!["org-keep".to_string()])
        );
        // An explicit empty array is a distinct (admin-error) state, preserved.
        assert_eq!(
            fold_force_login_org_pin(&[r#"{"forceLoginOrgUUID":[]}"#.to_string()]),
            ForceLoginOrgPin::EmptyArray
        );
        // An unparseable tier is skipped (fail-open), lower tier's pin kept.
        let tiers3 = vec![
            r#"{"forceLoginOrgUUID":"org-lo"}"#.to_string(),
            r#"{not json"#.to_string(),
        ];
        assert_eq!(
            fold_force_login_org_pin(&tiers3),
            ForceLoginOrgPin::Pinned(vec!["org-lo".to_string()])
        );
    }

    #[test]
    fn settings_json_typed_accessors() {
        let json = r#"{
            "forceLoginMethod": "console",
            "forceLoginOrgUUID": ["org-1", "org-2"]
        }"#;
        let s: SettingsJson = serde_json::from_str(json).unwrap();
        assert_eq!(
            s.force_login_method_parsed(),
            Some(ForceLoginMethod::Console)
        );
        assert_eq!(
            s.force_login_org_pin(),
            ForceLoginOrgPin::Pinned(vec!["org-1".to_string(), "org-2".to_string()])
        );
        // Out-of-set method ⇒ None (catch).
        let bad = r#"{"forceLoginMethod":"sso"}"#;
        let s2: SettingsJson = serde_json::from_str(bad).unwrap();
        assert_eq!(s2.force_login_method_parsed(), None);
    }
}
