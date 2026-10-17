//! Local bundled-skill admission, kept separate from provider or organization
//! policy services.
//!
//! Native 2.1.295's `R6()` checks the raw environment value with JavaScript
//! truthiness and disables bundled workflows when `disableBundledSkills` is
//! exactly `true`. Its locally registered `allow_bundled_skills` policy rule
//! has `onCacheMiss: "allow"`; an unresolved local verdict therefore follows
//! that cold-cache default. Hosts that later receive an explicit resolved
//! verdict can pass it here without changing the builder API.

use std::ffi::OsStr;

const DISABLE_BUNDLED_SKILLS_ENV: &str = "CLAUDE_CODE_DISABLE_BUNDLED_SKILLS";

/// Whether the process env or effective settings disable bundled skills and
/// workflows. Native tests the environment value with JS truthiness: any
/// nonempty string, including `"false"` or `"0"`, is true.
pub(crate) fn bundled_skills_disabled(disabled_by_settings: bool) -> bool {
    disabled_by_settings
        || js_environment_value_is_truthy(std::env::var_os(DISABLE_BUNDLED_SKILLS_ENV).as_deref())
}

/// Native's local `allow_bundled_skills` rule admits on a cache miss. `None`
/// means this host has no resolved local verdict; it is not a remote-policy
/// denial or an unregistered adapter.
pub(crate) fn local_policy_allows_bundled_skills(resolved_allow: Option<bool>) -> bool {
    resolved_allow.unwrap_or(true)
}

/// Whether the compiled workflow catalog should include bundled workflows.
pub(crate) fn bundled_workflows_admitted(
    disabled_by_settings: bool,
    resolved_policy_allow: Option<bool>,
) -> bool {
    bundled_workflows_admitted_for_inputs(
        disabled_by_settings,
        resolved_policy_allow,
        std::env::var_os(DISABLE_BUNDLED_SKILLS_ENV).as_deref(),
    )
}

fn bundled_workflows_admitted_for_inputs(
    disabled_by_settings: bool,
    resolved_policy_allow: Option<bool>,
    env_value: Option<&OsStr>,
) -> bool {
    !(disabled_by_settings || js_environment_value_is_truthy(env_value))
        && local_policy_allows_bundled_skills(resolved_policy_allow)
}

fn js_environment_value_is_truthy(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{
        bundled_workflows_admitted_for_inputs, js_environment_value_is_truthy,
        local_policy_allows_bundled_skills,
    };
    use std::ffi::OsStr;

    #[test]
    fn cold_policy_defaults_to_admit_but_explicit_deny_and_disable_still_win() {
        assert!(local_policy_allows_bundled_skills(None));
        assert!(local_policy_allows_bundled_skills(Some(true)));
        assert!(!local_policy_allows_bundled_skills(Some(false)));
        assert!(bundled_workflows_admitted_for_inputs(false, None, None));
        assert!(!bundled_workflows_admitted_for_inputs(
            false,
            Some(false),
            None
        ));
        assert!(!bundled_workflows_admitted_for_inputs(
            true,
            Some(true),
            None
        ));
    }

    #[test]
    fn disable_environment_uses_javascript_truthiness() {
        assert!(!js_environment_value_is_truthy(None));
        assert!(!js_environment_value_is_truthy(Some(OsStr::new(""))));
        assert!(js_environment_value_is_truthy(Some(OsStr::new("1"))));
        assert!(js_environment_value_is_truthy(Some(OsStr::new("false"))));
        assert!(js_environment_value_is_truthy(Some(OsStr::new("0"))));
        assert!(bundled_workflows_admitted_for_inputs(
            false,
            None,
            Some(OsStr::new("")),
        ));
        assert!(!bundled_workflows_admitted_for_inputs(
            false,
            None,
            Some(OsStr::new("false")),
        ));
    }
}
