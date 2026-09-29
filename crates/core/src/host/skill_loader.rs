//! Skill-loader seam (G5 subagent skill preloading).
//!
//! claude-code's `runAgent` preloads each skill named in an agent's frontmatter
//! `skills:` list into the child's INITIAL messages before the query loop
//! (`runAgent.ts:577-646`): it resolves the skill name (exact / plugin-prefix /
//! suffix), loads its prompt content (`skill.getPromptForCommand('', ctx)`), and
//! pushes a `isMeta` user message whose first block is
//! `formatSkillLoadingMetadata(skillName, skill.progressMessage)` followed by the
//! loaded content blocks.
//!
//! The agent runner cannot reach the skill registry directly: that lives in
//! `commands-core` / the `Skill` tool crate, which (transitively) depend on the
//! agent/tool stack — a direct dep would cycle. This LEAF trait
//! ([`SkillLoader`]) is the injection seam: the composition root (engine-desktop)
//! constructs a concrete impl over the live skill/command registry and hands it
//! to the spawner via `Arc<dyn SkillLoader>`; the runner calls
//! [`SkillLoader::resolve_and_load`] without taking the cyclic dep. When no
//! loader is wired (tests / minimal builds) the runner skips skill preloading
//! entirely, keeping the child's history byte-identical to legacy.

use crate::types::ContentBlock;
use async_trait::async_trait;

/// A resolved + loaded skill ready for injection into a child agent's initial
/// messages. Mirrors the data claude's `runAgent` skill-preload loop carries:
/// the (resolved) skill name for the loading-metadata block, the skill's
/// optional `progressMessage`, and the loaded prompt content blocks.
#[derive(Debug, Clone)]
pub struct SkillLoad {
    /// The skill name to render in the loading-metadata block (claude passes the
    /// ORIGINAL `skillName` from the frontmatter list to
    /// `formatSkillLoadingMetadata`, runAgent.ts:634).
    pub display_name: String,
    /// The skill's `progressMessage` frontmatter (claude
    /// `skill.progressMessage`); `None` ⇒ claude's default `"loading"`. Currently
    /// only carried for parity — `formatSkillLoadingMetadata` ignores it (the
    /// `_progressMessage` param is unused, processSlashCommand.tsx:786).
    pub progress_message: Option<String>,
    /// The loaded skill prompt content blocks (claude
    /// `await skill.getPromptForCommand('', toolUseContext)`). Pushed AFTER the
    /// leading metadata text block in the child's `isMeta` user message.
    pub content: Vec<ContentBlock>,
}

/// Resolves + loads a frontmatter skill name into injectable content for a child
/// agent's initial messages. See the module docs for the cycle-break rationale.
#[async_trait]
pub trait SkillLoader: Send + Sync {
    /// Resolve `skill_name` (claude `resolveSkillName`: exact match, then the
    /// agent's plugin prefix `pluginName:skill`, then a `:skill` suffix match —
    /// the concrete impl owns this strategy), load its prompt content, and return
    /// a [`SkillLoad`]. Returns `Ok(None)` when the skill cannot be resolved OR is not
    /// a prompt-based skill (claude logs a warning and `continue`s in both cases);
    /// the runner emits claude's exact warn string and skips it.
    ///
    /// `agent_type` is the child's `agentType` — its first `:`-segment is the
    /// plugin prefix the impl tries for the plugin-qualified resolution strategy.
    /// `cwd` overrides the parent session cwd for isolated/worktree children.
    /// Read/expansion failures return `Err`; they must not look like missing skills.
    async fn resolve_and_load(
        &self,
        skill_name: &str,
        agent_type: &str,
        cwd: Option<&std::path::Path>,
    ) -> Result<Option<SkillLoad>, String>;
}
