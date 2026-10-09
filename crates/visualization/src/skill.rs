//! The `visualize` skill text. One Markdown source serves every host; the
//! host's shell tool name is substituted when it has one, and the optional
//! validation step disappears when it does not.

const SKILL: &str = include_str!("../assets/skill/SKILL.md");
const SHELL_TOOL: &str = "{{SHELL_TOOL}}";
const IF_SHELL: &str = "<!-- if-shell -->\n";
const END_IF_SHELL: &str = "<!-- end-if-shell -->\n";

/// Skill name, `/visualize`.
pub const SKILL_NAME: &str = "visualize";

/// The skill Markdown (frontmatter included) for a host whose shell tool is
/// `shell_tool` (`Bash` on desktop, `Shell` on mobile), or none.
#[must_use]
pub fn skill_markdown(shell_tool: Option<&str>) -> String {
    let mut out = String::with_capacity(SKILL.len());
    let mut rest = SKILL;
    while let Some(start) = rest.find(IF_SHELL) {
        out.push_str(&rest[..start]);
        let after = &rest[start + IF_SHELL.len()..];
        let end = after
            .find(END_IF_SHELL)
            .expect("unterminated if-shell block in SKILL.md");
        if let Some(tool) = shell_tool {
            out.push_str(&after[..end].replace(SHELL_TOOL, tool));
        }
        rest = &after[end + END_IF_SHELL.len()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frontmatter(markdown: &str) -> &str {
        markdown
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n"))
            .map(|(frontmatter, _)| frontmatter)
            .unwrap()
    }

    /// Display columns: CJK and other wide characters count double.
    fn columns(text: &str) -> usize {
        text.chars()
            .map(|c| {
                if (c as u32) >= 0x1100 && !c.is_ascii() {
                    2
                } else {
                    1
                }
            })
            .sum()
    }

    #[test]
    fn frontmatter_follows_the_skill_contract() {
        let markdown = skill_markdown(Some("Bash"));
        let frontmatter = frontmatter(&markdown);
        assert!(frontmatter.contains("name: visualize\n"));
        assert!(frontmatter.contains("session-modes: chat, code"));
        for forbidden in [
            "allowed-tools",
            "context:",
            "agent:",
            "shell:",
            "background:",
        ] {
            assert!(!frontmatter.contains(forbidden), "{forbidden}");
        }
        let description = frontmatter
            .lines()
            .find_map(|line| line.strip_prefix("description: "))
            .unwrap();
        assert!(columns(description) <= 180, "{}", columns(description));
    }

    #[test]
    fn shell_step_is_substituted_or_omitted() {
        let desktop = skill_markdown(Some("Bash"));
        assert!(desktop.contains("with the `Bash` tool"));
        assert!(!desktop.contains("{{"), "no placeholder survives");
        assert!(!desktop.contains("if-shell"));
        let mobile = skill_markdown(Some("Shell"));
        assert!(mobile.contains("with the `Shell` tool"));
        let none = skill_markdown(None);
        assert!(!none.contains("file_path"));
        assert!(!none.contains("if-shell") && !none.contains("{{"));
        assert!(none.contains("3. Fix whatever the tool reports"));
    }

    #[test]
    fn body_has_no_embedded_shell_syntax() {
        for markdown in [skill_markdown(Some("Shell")), skill_markdown(None)] {
            assert!(!markdown.contains("```!"));
            assert!(!markdown.contains("!`"));
        }
    }
}
