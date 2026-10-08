//! [`DesktopForkResumeGate`] — the production forked-skill resume gate.
//!
//! Wires the three pieces the check needs, which live in three different
//! places: the scoping sidecars on disk (`session::forked_skill`), the fork
//! identity the live task record carries (threaded in by the caller), and the
//! skill registry that says whether the named skill is STILL fork-capable.
//!
//! The gate is consulted by [`tasks::handlers::LocalAgentHandler`] before it
//! resumes a parked background agent. It refuses in six distinct states, each
//! carrying claude-code's byte-exact reason and message — see
//! [`session::forked_skill::check_scoping_provenance`] and
//! [`session::forked_skill::check_fork_capable`].

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use lingxi_core::host::fork_resume_gate::ForkResumeGate;

/// Resolves whether a skill name still names a FORK-CAPABLE skill.
///
/// Claude's test is `command.type === "prompt" && (context === "fork" ||
/// getContext !== undefined)` — a skill deleted, renamed, or edited to drop
/// `context: fork` no longer supplies the lists the fork ran under.
#[async_trait]
pub trait ForkCapableSkills: Send + Sync {
    /// Whether `skill_name` currently resolves to a fork-capable skill.
    async fn is_fork_capable(&self, skill_name: &str) -> bool;
}

/// A [`ForkCapableSkills`] backed by the live [`CommandRegistry`], bound AFTER
/// construction.
///
/// The `LocalAgentHandler` (and therefore its gate) is registered before the
/// command registry exists — the same registration cycle
/// `RegistryStatusSink` solves — so this holds a set-once cell bound at the
/// composition root once the registry is built.
///
/// An UNBOUND cell reports NOT fork-capable, which refuses the resume. That is
/// the fail-closed direction: a gate that cannot consult the registry must not
/// wave a forked skill through.
#[derive(Default)]
pub struct RegistryForkCapableSkills {
    registry: std::sync::OnceLock<Arc<tokio::sync::RwLock<command_api::CommandRegistry>>>,
}

impl RegistryForkCapableSkills {
    /// A new, unbound resolver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind the live registry. Idempotent — a second bind is ignored.
    pub fn bind(&self, registry: Arc<tokio::sync::RwLock<command_api::CommandRegistry>>) {
        let _ = self.registry.set(registry);
    }
}

#[async_trait]
impl ForkCapableSkills for RegistryForkCapableSkills {
    async fn is_fork_capable(&self, skill_name: &str) -> bool {
        let Some(registry) = self.registry.get() else {
            return false;
        };
        let reg = registry.read().await;
        // Claude's test: the command must still resolve, and still declare
        // fork execution. A renamed / deleted / edited-to-inline skill fails.
        reg.resolve(skill_name)
            .and_then(|cmd| match &cmd.kind {
                command_api::SlashCommandKind::Markdown { frontmatter, .. }
                | command_api::SlashCommandKind::Plugin { frontmatter, .. }
                | command_api::SlashCommandKind::Bundled { frontmatter, .. } => {
                    Some(frontmatter.context.as_deref() == Some("fork"))
                }
                _ => None,
            })
            .unwrap_or(false)
    }
}

/// The production gate.
pub struct DesktopForkResumeGate {
    /// This session's `subagents/` directory — where each background agent's
    /// transcript lives, and therefore where its scoping sidecars sit (beside
    /// `agent-<id>.jsonl`).
    pub subagents_dir: PathBuf,
    /// The live skill registry.
    pub skills: Arc<dyn ForkCapableSkills>,
}

#[async_trait]
impl ForkResumeGate for DesktopForkResumeGate {
    async fn check_resume(
        &self,
        agent_id: lingxi_core::types::AgentId,
        task_forked_skill_name: Option<&str>,
    ) -> Result<(), String> {
        let jsonl = session::forked_skill::agent_transcript_path(
            &self.subagents_dir,
            &agent_id.to_string(),
        );
        let status = session::forked_skill::read_scoping(&jsonl).await;

        // The provenance marker is only consulted on the COLD path (no live
        // task record to corroborate against), so read it only when it can
        // matter — a hot resume of an ordinary agent stays one stat call.
        let witness = if task_forked_skill_name.is_none()
            && matches!(status, session::forked_skill::ScopingStatus::Valid(_))
        {
            session::forked_skill::read_marker_skill_name(&jsonl).await
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
        .map_err(|r| r.message)?;

        // Not a forked skill — resume normally.
        let Some(scoping) = scoping else {
            return Ok(());
        };

        let capable = self.skills.is_fork_capable(&scoping.skill_name).await;
        session::forked_skill::check_fork_capable(&label, &scoping.skill_name, capable)
            .map_err(|r| r.message)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use session::forked_skill::{ForkedSkillScoping, write_fork_records};

    struct Skills(bool);
    #[async_trait]
    impl ForkCapableSkills for Skills {
        async fn is_fork_capable(&self, _skill_name: &str) -> bool {
            self.0
        }
    }

    fn gate(dir: &std::path::Path, capable: bool) -> DesktopForkResumeGate {
        DesktopForkResumeGate {
            subagents_dir: dir.to_path_buf(),
            skills: Arc::new(Skills(capable)),
        }
    }

    fn scoping(skill: &str) -> ForkedSkillScoping {
        ForkedSkillScoping {
            skill_name: skill.into(),
            attribution_name: skill.into(),
            effort: None,
            frozen_command_denies: None,
        }
    }

    /// An agent that never forked resumes untouched — the gate must not become
    /// a tax on every background agent.
    #[tokio::test]
    async fn an_ordinary_agent_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        assert!(gate(dir.path(), true).check_resume(id, None).await.is_ok());
    }

    /// The happy path: the record is on disk, the task record agrees, and the
    /// skill is still fork-capable.
    #[tokio::test]
    async fn a_corroborated_fork_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        write_fork_records(
            &session::forked_skill::agent_transcript_path(dir.path(), &id.to_string()),
            &scoping("review"),
        )
        .await
        .unwrap();
        assert!(
            gate(dir.path(), true)
                .check_resume(id, Some("review"))
                .await
                .is_ok()
        );
    }

    /// Deleting the scoping record does NOT downgrade the fork to an unscoped
    /// resume — the provenance marker survives and turns the deletion into a
    /// refusal. This is the whole reason the marker exists.
    #[tokio::test]
    async fn deleting_the_scoping_record_refuses_rather_than_widening() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        let jsonl = session::forked_skill::agent_transcript_path(dir.path(), &id.to_string());
        write_fork_records(&jsonl, &scoping("review"))
            .await
            .unwrap();
        tokio::fs::remove_file(session::forked_skill::forked_skill_paths(&jsonl).scoping)
            .await
            .unwrap();

        let err = gate(dir.path(), true)
            .check_resume(id, Some("review"))
            .await
            .unwrap_err();
        assert!(
            err.contains("ran as a forked skill but its scoping record is missing"),
            "{err}"
        );
    }

    /// A scoping record naming a DIFFERENT skill than the task record belongs
    /// to a different fork; resuming under it would apply the wrong scoping.
    #[tokio::test]
    async fn a_mismatched_identity_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        write_fork_records(
            &session::forked_skill::agent_transcript_path(dir.path(), &id.to_string()),
            &scoping("review"),
        )
        .await
        .unwrap();
        let err = gate(dir.path(), true)
            .check_resume(id, Some("deploy"))
            .await
            .unwrap_err();
        assert!(err.contains("does not match its task record"), "{err}");
    }

    /// A skill edited to drop `context: fork` no longer supplies the lists the
    /// fork ran under, so there is nothing to re-establish.
    #[tokio::test]
    async fn a_skill_that_lost_fork_capability_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        write_fork_records(
            &session::forked_skill::agent_transcript_path(dir.path(), &id.to_string()),
            &scoping("review"),
        )
        .await
        .unwrap();
        let err = gate(dir.path(), false)
            .check_resume(id, Some("review"))
            .await
            .unwrap_err();
        assert!(
            err.contains("no longer resolves to a fork-capable skill"),
            "{err}"
        );
    }

    /// A corrupt record is a refusal, never "this agent never forked".
    #[tokio::test]
    async fn a_corrupt_record_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        let jsonl = session::forked_skill::agent_transcript_path(dir.path(), &id.to_string());
        tokio::fs::write(
            session::forked_skill::forked_skill_paths(&jsonl).scoping,
            "{ not json",
        )
        .await
        .unwrap();
        let err = gate(dir.path(), true)
            .check_resume(id, None)
            .await
            .unwrap_err();
        assert!(
            err.contains("malformed forked-skill scoping record"),
            "{err}"
        );
    }

    /// COLD path — no live task record. The provenance marker is the only
    /// witness, so a planted scoping record with no marker is refused.
    #[tokio::test]
    async fn a_cold_resume_without_a_witness_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        let jsonl = session::forked_skill::agent_transcript_path(dir.path(), &id.to_string());
        // Write ONLY the scoping record — no marker.
        tokio::fs::write(
            session::forked_skill::forked_skill_paths(&jsonl).scoping,
            serde_json::to_string(&scoping("review")).unwrap(),
        )
        .await
        .unwrap();
        let err = gate(dir.path(), true)
            .check_resume(id, None)
            .await
            .unwrap_err();
        assert!(
            err.contains("no matching provenance-marker witness"),
            "{err}"
        );
    }

    /// COLD path with a marker that agrees: the identity is corroborated, so
    /// the resume proceeds.
    #[tokio::test]
    async fn a_cold_resume_with_a_matching_witness_proceeds() {
        let dir = tempfile::tempdir().unwrap();
        let id = lingxi_core::types::AgentId::new();
        write_fork_records(
            &session::forked_skill::agent_transcript_path(dir.path(), &id.to_string()),
            &scoping("review"),
        )
        .await
        .unwrap();
        assert!(gate(dir.path(), true).check_resume(id, None).await.is_ok());
    }
}
