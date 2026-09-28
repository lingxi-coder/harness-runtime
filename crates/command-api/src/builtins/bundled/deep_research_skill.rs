//! Manual `/deep-research` entry point for the bundled workflow.

use crate::BundledPromptFn;

pub(crate) const DESCRIPTION: &str =
    "Run the built-in multi-source deep-research workflow for a user-supplied question.";
pub(crate) const ARGUMENT_HINT: &str = "<question>";

/// Build the short launcher instruction. The research content itself lives in
/// the immutable built-in workflow registry and is launched through the same
/// `Workflow({name: ...})` path as a direct tool invocation.
pub struct DeepResearchPromptFn;

impl BundledPromptFn for DeepResearchPromptFn {
    fn build(&self, args: &str) -> String {
        let encoded = serde_json::to_string(args.trim()).expect("string serialization");
        format!(
            "The user manually requested the built-in deep-research workflow. Call the Workflow \
tool exactly once with {{\"name\":\"deep-research\",\"args\":{encoded}}}. Do not replace it \
with an inline workflow and do not begin the research in this conversation."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_unicode_and_quotes_as_workflow_args() {
        let prompt = DeepResearchPromptFn.build("  为什么 \"Rust\" 安全？  ");
        assert!(prompt.contains("\"name\":\"deep-research\""));
        assert!(prompt.contains(r#""args":"为什么 \"Rust\" 安全？""#));
        assert!(prompt.contains("exactly once"));
    }
}
