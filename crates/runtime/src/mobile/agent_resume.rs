//! Mobile composition of the shared forked-skill resume checks.
use std::sync::Arc;

pub(crate) struct MobileForkResumeGate {
    pub spawner: Arc<dyn lingxi_core::host::SubagentSpawner>,
    pub commands: Arc<tokio::sync::RwLock<command_api::CommandRegistry>>,
}

#[async_trait::async_trait]
impl lingxi_core::host::fork_resume_gate::ForkResumeGate for MobileForkResumeGate {
    async fn check_resume(
        &self,
        agent_id: lingxi_core::types::AgentId,
        task_forked_skill_name: Option<&str>,
    ) -> Result<(), String> {
        let transcript = self
            .spawner
            .transcript_path(agent_id)
            .ok_or_else(|| "agent transcript path unavailable".to_string())?;
        let status = session::forked_skill::read_scoping(&transcript).await;
        let witness = if task_forked_skill_name.is_none()
            && matches!(status, session::forked_skill::ScopingStatus::Valid(_))
        {
            session::forked_skill::read_marker_skill_name(&transcript).await
        } else {
            None
        };
        let label = agent_id.to_string();
        let scoping = session::forked_skill::check_scoping_provenance(
            &label,
            &status,
            task_forked_skill_name,
            witness.as_deref(),
        )
        .map_err(|reason| reason.message)?;
        let Some(scoping) = scoping else {
            return Ok(());
        };
        let registry = self.commands.read().await;
        let capable = registry
            .resolve(&scoping.skill_name)
            .and_then(|command| match &command.kind {
                command_api::SlashCommandKind::Markdown { frontmatter, .. }
                | command_api::SlashCommandKind::Plugin { frontmatter, .. }
                | command_api::SlashCommandKind::Bundled { frontmatter, .. } => {
                    Some(frontmatter.context.as_deref() == Some("fork"))
                }
                _ => None,
            })
            .unwrap_or(false);
        session::forked_skill::check_fork_capable(&label, &scoping.skill_name, capable)
            .map_err(|reason| reason.message)
    }
}
