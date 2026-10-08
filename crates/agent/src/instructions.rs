//! Instruction context selection for every subagent entrypoint.
//!
//! Oracle: Claude Code 2.1.286, native SHA-256
//! 75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433,
//! `src_191018893.js` Lv / Fl. The initial Gv load propagates failure before
//! the child starts. A later failed managed projection retains the full
//! context; a successful empty projection removes only the instruction key.

use crate::context::SubagentContext;
use crate::definition::AgentSource;
use lingxi_core::host::instructions::{InstructionContext, InstructionRendering, InstructionScope};
use lingxi_core::types::SettingsScope;

/// xd uses the latest native snapshot only; an invalid latest snapshot does
/// not fall back to an older hint. The optional hint itself is schema-caught.
pub(crate) fn cold_rendering(
    attachments: &[serde_json::Value],
    metadata_enabled: bool,
) -> InstructionRendering {
    if !metadata_enabled {
        return InstructionRendering::Announced;
    }
    lingxi_core::host::instruction_announcements::latest_snapshot_context_rendering(attachments)
        .unwrap_or(InstructionRendering::Announced)
}

pub(crate) async fn resolve(ctx: &SubagentContext) -> Result<InstructionContext, String> {
    // A cold resume replays messages, not the prior process's Gv cache or Ye
    // cursor. The live runner keeps this context across its parked turn sets.
    let cold_resume = ctx.resumed_history.is_some();
    if ctx.instruction_context_is_override && !cold_resume {
        let mut explicit = ctx.instruction_context.clone();
        // Lv bypasses Fl for an explicit override.userContext and selects
        // managedInstructionsOnly=false. The provider's trusted global mode
        // still applies during lazy discovery.
        explicit.managed_instructions_only = false;
        return Ok(explicit);
    }
    let cwd = ctx.cwd.as_deref().unwrap_or(&ctx.hook_cwd);
    let mut original = if cold_resume {
        InstructionContext {
            rendering: cold_rendering(
                &ctx.instruction_context.announcement_history,
                telemetry::feature_flags::flag_bool("tengu_foamy_spring", true),
            ),
            announcement_history: ctx.instruction_context.announcement_history.clone(),
            ..Default::default()
        }
    } else {
        ctx.instruction_context.clone()
    };
    // A fresh child resolves Gv independently. Its parent's omit-derived go
    // flag is not among the options Lv copies into the child's Io.
    original.managed_instructions_only = false;
    if !cold_resume {
        original.announcement_history.clear();
    }
    if let Some(provider) = &ctx.instruction_provider {
        // Lv awaits Gv in its initial Promise.all, before Fl's best-effort
        // managed projection. Its rejection must not reuse inherited policy.
        let loaded = provider.load(cwd, InstructionScope::Full).await?;
        // Gv is the whole current root userContext. Inherit neither keys nor
        // ordering from a prior account/project snapshot; only an explicit
        // fresh override above bypasses this authoritative selection.
        let announcement_history = original.announcement_history;
        let rendering = original.rendering;
        original = loaded;
        original.announcement_history = announcement_history;
        if cold_resume {
            original.rendering = rendering;
        }
    }
    if !ctx.agent_definition.omit_instructions {
        return Ok(original);
    }
    if matches!(
        ctx.agent_definition.source,
        AgentSource::BuiltIn | AgentSource::Settings(SettingsScope::Managed)
    ) {
        original.user_context.remove("instructions");
        return Ok(original);
    }
    let Some(provider) = &ctx.instruction_provider else {
        return Ok(original);
    };
    let Ok(managed) = provider.load(cwd, InstructionScope::ManagedOnly).await else {
        return Ok(original);
    };
    // Fl changes only instructions on the already-selected Full object. A root
    // account/project change while the managed projection awaits must not
    // replace the other fields or their insertion order in this child.
    let managed_body = managed
        .user_context
        .get("instructions")
        .filter(|body| !body.is_empty())
        .cloned();
    let has_managed = managed_body.is_some();
    original.user_context.remove("instructions");
    if let Some(body) = managed_body {
        original.user_context.insert("instructions".into(), body);
    }
    original.sent_paths = managed.sent_paths;
    original.project_instruction_bodies = managed.project_instruction_bodies;
    original.instructions_root = managed.instructions_root;
    original.managed_instructions_only = has_managed;
    Ok(original)
}
