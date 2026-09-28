//! The `workflow-authoring` bundled skill — port of Claude Code 2.1.267's
//! `dCr()` registrar (`src_185945034.js`).
//!
//! ```js
//! function eFt(){ return [{type:"text", text: wpn()}] }
//! function dCr(){ uo({
//!   name: Iv,                                   // "workflow-authoring"
//!   description: `Reference for writing a ${lu} tool script (…)`,
//!   menuDescription: "Load the reference for writing Workflow tool scripts",
//!   userInvocable: !0,
//!   isEnabled: () => qc(),
//!   async getPromptForCommand(){ return eFt() } }) }
//! ```
//!
//! 2.1.267 moved the 17 KB script-writing reference out of the Workflow tool
//! description and behind this skill, cutting the per-request footprint from
//! about 5.7k tokens to 1k. The text itself lives with the tool
//! (`workflow::description`), which owns both byte-locked
//! oracle documents and the single divergence register that edits them.
//!
//! ⚠️ Divergence: upstream gates registration on `isEnabled: () => qc()`
//! (workflows enabled). This port registers unconditionally, because
//! `register_bundled_skills` does not receive the managed workflow-disable
//! setting and a reference the user can still read while workflows are off is
//! inert, not harmful. Gating it on a predicate the registration site cannot
//! actually see would risk the dangerous direction instead: the skill silently
//! absent while the Workflow description claims it is loadable, which is the
//! one state the reachability invariant forbids.

use crate::BundledPromptFn;

/// Upstream's `description`, with `${lu}` resolved to the tool name.
///
/// The closing clause is load-bearing: the Workflow tool refuses to run without
/// an explicit user opt-in, and reading the reference is not one. Without it a
/// model that loaded the skill could read its own preparation as permission.
pub(crate) const WORKFLOW_AUTHORING_DESCRIPTION: &str = "Reference for writing a Workflow tool script (script API and gotchas, resume, quality patterns, worked examples). Load before authoring a script for a workflow the user already opted into; it does not itself authorize running one.";

pub(crate) const WORKFLOW_AUTHORING_MENU_DESCRIPTION: &str =
    "Load the reference for writing Workflow tool scripts";

/// `eFt()` — the reference, rendered for this session.
pub struct WorkflowAuthoringPromptFn;

impl BundledPromptFn for WorkflowAuthoringPromptFn {
    fn build(&self, _args: &str) -> String {
        workflow::description::authoring_skill_prompt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The skill exists to carry the hooks the tool description no longer
    /// inlines. If it served anything less, moving them out would have made
    /// them unreachable.
    #[test]
    fn the_skill_serves_the_whole_authoring_reference() {
        let body = WorkflowAuthoringPromptFn.build("");
        assert!(body.starts_with("# Workflow authoring reference"));
        for hook in [
            "- agent(prompt: string",
            "- pipeline(items,",
            "- parallel(thunks:",
            "- workflow(nameOrRef:",
        ] {
            assert!(body.contains(hook), "the reference must document {hook:?}");
        }
        assert!(body.len() > 15_000, "got {} bytes", body.len());
    }

    /// Loading the reference must not read as authorization to run a workflow.
    #[test]
    fn the_description_denies_that_loading_it_is_an_opt_in() {
        assert!(WORKFLOW_AUTHORING_DESCRIPTION.contains("it does not itself authorize running one"));
        assert!(WORKFLOW_AUTHORING_DESCRIPTION.contains("already opted into"));
    }
}
