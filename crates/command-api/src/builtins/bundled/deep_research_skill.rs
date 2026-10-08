//! Manual `/deep-research` entry point for the bundled workflow.

use crate::model::{trim_source_text, BundledPromptFn};

pub(crate) const DESCRIPTION: &str =
    "Run the built-in multi-source deep-research workflow for a user-supplied question.";
pub(crate) const ARGUMENT_HINT: &str = "<question>";

/// Build the short launcher instruction. The research content itself lives in
/// the immutable built-in workflow registry and is launched through the same
/// `Workflow({name: ...})` path as a direct tool invocation.
pub struct DeepResearchPromptFn;

impl BundledPromptFn for DeepResearchPromptFn {
    fn build(
        &self,
        args: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        let args = trim_source_text(args);
        let encoded = args
            .to_json_string()
            .expect("root string JSON serialization");
        let text = format!(
            "The user manually requested the built-in deep-research workflow. Call the Workflow \
            tool exactly once with {{\"name\":\"deep-research\",\"args\":{encoded}}}. Do not replace it \
            with an inline workflow and do not begin the research in this conversation."
        );
        text.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceTextTestExt;

    #[test]
    fn preserves_unicode_and_quotes_as_workflow_args() {
        let prompt = DeepResearchPromptFn.build(&"  为什么 \"Rust\" 安全？  ".into());
        assert!(prompt.contains("\"name\":\"deep-research\""));
        assert!(prompt.contains(r#""args":"为什么 \"Rust\" 安全？""#));
        assert!(prompt.contains("exactly once"));
    }
}
