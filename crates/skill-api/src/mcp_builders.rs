//! Helpers for synthesizing [`Skill`]s from MCP server tool descriptions.

use crate::model::{LoadedFrom, Skill, SkillFrontmatter, SkillSource};
use lingxi_core::host::McpToolDto;

/// Build a [`Skill`] whose triggers are derived from an MCP tool's name and
/// whose content mirrors the tool's description.
#[must_use]
pub fn skill_from_mcp_tool(tool: &McpToolDto) -> Skill {
    Skill {
        name: tool.full_name.clone(),
        description: tool.description.clone(),
        frontmatter: SkillFrontmatter {
            name: tool.full_name.clone(),
            description: tool.description.clone(),
            triggers: vec![tool.tool_name.to_lowercase()],
            ..Default::default()
        },
        content: tool.description.clone(),
        source: SkillSource::Mcp {},
        loaded_from: LoadedFrom::Mcp,
        plugin_id: None,
        file_path: "<mcp>".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    #[test]
    fn mcp_skill_keeps_description_units_in_model_fields() {
        let description = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&[b'A' as u16, 0xd800, b'B' as u16]),
            vec![b'A' as u16, 0xd800, b'B' as u16],
        )
        .expect("valid source string projection");
        let tool = McpToolDto {
            server_name: "server".into(),
            tool_name: "tool".into(),
            description: description.clone(),
            input_schema: Utf16JsonProjection::parse("{}").expect("object schema"),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            meta: None,
            full_name: "mcp__server__tool".into(),
            search_hint: None,
            always_load: None,
            requires_user_interaction: false,
        };

        let skill = skill_from_mcp_tool(&tool);
        let expected = vec![b'A' as u16, 0xd800, b'B' as u16];
        assert_eq!(skill.description.string_units(""), Some(expected.clone()));
        assert_eq!(skill.content.string_units(""), Some(expected.clone()));
        assert_eq!(
            skill.frontmatter.description.string_units(""),
            Some(expected)
        );

        let encoded = serde_json::to_string(&skill).expect("skill DTO serializes as JSON");
        let decoded: Skill = serde_json::from_str(&encoded).expect("skill DTO keeps raw strings");
        assert_eq!(
            decoded.description.string_units(""),
            Some(vec![u16::from(b'A'), 0xd800, u16::from(b'B')])
        );
        assert_eq!(
            decoded.frontmatter.description.string_units(""),
            Some(vec![u16::from(b'A'), 0xd800, u16::from(b'B')])
        );
        assert_eq!(
            decoded.content.string_units(""),
            Some(vec![u16::from(b'A'), 0xd800, u16::from(b'B')])
        );
    }
}
