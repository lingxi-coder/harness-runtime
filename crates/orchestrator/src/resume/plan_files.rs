//! Recover the plan file from typed transcript metadata, never model prose.

use super::{ConversationOrchestrator, JsonlMessage};
use lingxi_core::host::plan_files::{is_valid_plan_slug, PlanFileIdentity};

pub(super) fn plan_slug_from_messages(messages: &[JsonlMessage]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        (!message.is_sidechain)
            .then_some(message.slug.as_deref())
            .flatten()
            .filter(|slug| is_valid_plan_slug(slug))
            .map(str::to_owned)
    })
}

impl ConversationOrchestrator {
    /// Follow an actual project-root activation while retaining the session's
    /// restored slug. Hosts call this after restoring the project root, never
    /// for an ordinary shell directory change.
    pub fn rebind_plan_directory_to_project_root(&self, project_root: &std::path::Path) {
        let Some(files) = self.config.plan_files.as_ref() else {
            return;
        };
        if let Some(mut identity) = files.identity() {
            identity.plans_dir =
                Self::plans_dir(project_root, self.config.plans_directory.as_deref());
            files.publish(identity);
        }
    }

    /// Resolve before committing a session switch. Publishing is separate so a
    /// rejected resume leaves the old session's permission carve-out untouched.
    pub(crate) fn prepare_plan_file_identity(
        &self,
        slug: Option<&str>,
    ) -> Option<PlanFileIdentity> {
        self.config
            .plan_files
            .as_ref()?
            .identity()
            .map(|identity| identity.for_session(slug))
    }

    pub(crate) fn install_plan_file_identity(&self, identity: Option<PlanFileIdentity>) {
        if let (Some(files), Some(identity)) = (self.config.plan_files.as_ref(), identity) {
            files.publish(identity);
        }
    }

    pub(crate) fn restore_plan_file_slug(&self, slug: Option<&str>) {
        if let Some(slug) = slug.filter(|slug| is_valid_plan_slug(slug)) {
            self.install_plan_file_identity(self.prepare_plan_file_identity(Some(slug)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(slug: Option<&str>, sidechain: bool) -> JsonlMessage {
        serde_json::from_value(serde_json::json!({
            "type":"user",
            "uuid":uuid::Uuid::new_v4().to_string(),
            "parentUuid":null,
            "sessionId":uuid::Uuid::new_v4().to_string(),
            "isSidechain":sidechain,
            "slug":slug,
            "message":{"role":"user","content":"Do not infer a plan path from this text"}
        }))
        .unwrap()
    }

    #[test]
    fn the_last_main_plan_slug_survives_jsonl_round_trip() {
        let original = row(Some("quiet-editing-otter"), false);
        let encoded = serde_json::to_string(&original).unwrap();
        let reloaded: JsonlMessage = serde_json::from_str(&encoded).unwrap();
        let messages = [
            row(Some("old-session-plan"), false),
            reloaded,
            row(Some("child-plan"), true),
            row(Some("../../outside"), false),
            row(None, false),
        ];
        assert_eq!(
            plan_slug_from_messages(&messages).as_deref(),
            Some("quiet-editing-otter")
        );
    }

    #[test]
    fn absent_or_invalid_metadata_does_not_turn_model_text_into_a_plan_path() {
        assert_eq!(
            plan_slug_from_messages(&[row(None, false), row(Some("../outside"), false)]),
            None
        );
    }

    #[tokio::test]
    async fn project_activation_rebases_the_configured_directory_and_keeps_the_slug() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use lingxi_core::host::plan_files::PlanFileMatcher;
        use std::sync::Arc;

        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let files = Arc::new(PlanFileMatcher::with_identity(PlanFileIdentity {
            plans_dir: first.join("planning"),
            slug: "restored-plan".into(),
            workshop_enabled: false,
        }));
        let orch = ConversationOrchestrator::new(
            crate::OrchestratorConfig {
                plans_directory: Some("planning".into()),
                plan_files: Some(files.clone()),
                ..Default::default()
            },
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            first,
        );
        orch.rebind_plan_directory_to_project_root(&second);
        let expected = second.join("planning/restored-plan.md");
        assert_eq!(files.plan_file(None), Some(expected.clone()));
        assert!(files.matches(&expected, None, false));
        orch.session_cwd.change_cwd(second.join("src"));
        assert_eq!(files.plan_file(None), Some(expected));
    }
}
