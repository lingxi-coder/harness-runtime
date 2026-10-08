//! Shared Bash/mobile-shell precommit wording and session latch (CC 2.1.286).

use crate::tool_trait::BashPrecommitSkills;
use std::sync::{Arc, Mutex};

/// Clones share one session decision. Clear/resume changes the registry-owned
/// generation, including when the same tool object serves the new session.
#[derive(Clone, Debug, Default)]
pub struct BashPrecommitLatch(Arc<Mutex<Option<(u64, bool)>>>);

impl BashPrecommitLatch {
    /// Resolve the feature flag only for the first Git-enabled description in
    /// this session. `sFo()` is hard false in the external reference.
    #[must_use]
    pub fn suggestion(&self, skills: BashPrecommitSkills, generation: u64) -> String {
        if !include_git_instructions() {
            return String::new();
        }
        let mut latch = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let enabled = match *latch {
            Some((previous, enabled)) if previous == generation => enabled,
            _ => {
                let enabled = telemetry::flag_bool("tengu_polished_tulip", true);
                *latch = Some((generation, enabled));
                enabled
            }
        };
        suggestion(skills, enabled)
    }

    /// Current decision, or unset before the first Git-enabled description.
    #[must_use]
    pub fn latched_value(&self) -> Option<bool> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map(|(_, value)| value)
    }
}

/// The defined disable env value wins in both directions, then the setting.
#[must_use]
pub fn include_git_instructions() -> bool {
    std::env::var("LINGXI_DISABLE_GIT_INSTRUCTIONS").map_or_else(
        |_| lingxi_core::host::session_flags::include_git_instructions().unwrap_or(true),
        |value| !lingxi_core::host::env::is_env_truthy(Some(&value)),
    )
}

/// Exact `_It` sentence and conjunctions, including the docs/tests exclusion.
#[must_use]
pub fn suggestion(skills: BashPrecommitSkills, enabled: bool) -> String {
    if !enabled {
        return String::new();
    }
    let mut names = Vec::new();
    if skills.custom_verify {
        names.push("`/verify`");
    }
    if skills.custom_simplify {
        names.push("`/simplify`");
    }
    if skills.code_review {
        names.push("`/code-review medium`");
    }
    let names = match names.as_slice() {
        [] => return String::new(),
        [one] => (*one).to_string(),
        [one, two] => format!("{one} and {two}"),
        [one, two, three] => format!("{one}, {two}, and {three}"),
        _ => unreachable!("the oracle exposes exactly three precommit skills"),
    };
    format!("Always run {names} right before the `commit` command (never for docs or tests).")
}
