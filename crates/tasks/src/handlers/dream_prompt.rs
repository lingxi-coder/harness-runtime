//! Exact Claude Code 2.1.319 dream prompt, with the owning product's branding.

use crate::task_trait::DreamPromptContext;

const PROMPT: &str = include_str!("templates/dream-2.1.319.md");
const TEAM: &str = include_str!("templates/dream-team-2.1.319.md");
const MCP: &str = include_str!("templates/dream-mcp-2.1.319.md");

/// Render native `Q` without trimming, escaping, or recursively substituting
/// caller data. The native sixth argument is currently unused.
#[must_use]
pub fn render_consolidation_prompt(context: &DreamPromptContext) -> String {
    let additional = context
        .additional_context
        .as_deref()
        .filter(|extra| !extra.is_empty())
        .map_or_else(String::new, |extra| {
            format!("\n\n## Additional context\n\n{extra}")
        });
    let fields = [
        ("{memory_directory}", context.memory_directory.as_str()),
        (
            "{transcript_directory}",
            context.transcript_directory.as_str(),
        ),
        ("{team_memory}", if context.team_memory { TEAM } else { "" }),
        ("{mcp_memory}", if context.mcp_memory { MCP } else { "" }),
        ("{additional_context}", additional.as_str()),
    ];
    let mut output = String::with_capacity(
        PROMPT.len()
            + context.memory_directory.len()
            + context.transcript_directory.len()
            + additional.len(),
    );
    let mut remaining = PROMPT;
    while let Some((offset, placeholder, value)) = fields
        .iter()
        .filter_map(|(placeholder, value)| {
            remaining
                .find(placeholder)
                .map(|offset| (offset, *placeholder, *value))
        })
        .min_by_key(|(offset, _, _)| *offset)
    {
        output.push_str(&remaining[..offset]);
        output.push_str(value);
        remaining = &remaining[offset + placeholder.len()..];
    }
    output.push_str(remaining);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_native_dream_render_cases_match_exact_bytes() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/dream_319.json"
        ))
        .unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 18);
        for (index, case) in cases.iter().enumerate() {
            let input = &case["input"];
            let context = DreamPromptContext {
                memory_directory: input["memory_directory"].as_str().unwrap().to_owned(),
                transcript_directory: input["transcript_directory"].as_str().unwrap().to_owned(),
                additional_context: Some(input["additional_context"].as_str().unwrap().to_owned()),
                team_memory: input["team_memory"].as_bool().unwrap(),
                mcp_memory: input["mcp_memory"].as_bool().unwrap(),
            };
            assert_eq!(
                render_consolidation_prompt(&context).as_bytes(),
                case["expected"].as_str().unwrap().as_bytes(),
                "native Q case {index} (including its currently unused sixth argument)"
            );
        }
    }
}
