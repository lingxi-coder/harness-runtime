//! First-party account identity and context invalidation share one root owner.

use async_trait::async_trait;
use lingxi_core::host::auth::{AccountChangeObserver, AuthHandle};
use lingxi_core::host::instructions::InstructionRefreshReason;
use orchestrator::prompt::memory_block::InstructionUserEmailProvider;
use orchestrator::ConversationOrchestrator;
use std::sync::{Arc, OnceLock, Weak};

/// Reads the native account independently from other LLM provider identities.
pub(crate) struct InstructionIdentity {
    auth: Arc<dyn AuthHandle>,
    root: OnceLock<Weak<ConversationOrchestrator>>,
}

impl InstructionIdentity {
    pub(crate) fn new(auth: Arc<dyn AuthHandle>) -> Arc<Self> {
        let identity = Arc::new(Self {
            auth,
            root: OnceLock::new(),
        });
        let observer: Arc<dyn AccountChangeObserver> = identity.clone();
        identity
            .auth
            .register_account_change_observer(Arc::downgrade(&observer));
        identity
    }

    /// Bind once after composition publishes the exact owning orchestrator.
    pub(crate) fn bind_root(&self, root: &Arc<ConversationOrchestrator>) {
        assert!(
            self.root.set(Arc::downgrade(root)).is_ok(),
            "instruction identity belongs to one root"
        );
    }
}

#[async_trait]
impl InstructionUserEmailProvider for InstructionIdentity {
    async fn current_user_email(&self) -> Option<String> {
        self.auth.current_user().await.map(|user| user.email)
    }
}

impl AccountChangeObserver for InstructionIdentity {
    fn account_changed(&self) {
        if let Some(root) = self.root.get().and_then(Weak::upgrade) {
            root.invalidate_instruction_context(InstructionRefreshReason::AccountChange);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::auth::{AuthError, LoginInfo};
    use orchestrator::prompt::{MemoryFile, MemoryHierarchyProvider};
    use orchestrator::test_support::{
        mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream,
        NoOpPermissionGate,
    };
    use orchestrator::OrchestratorConfig;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Account {
        user: Mutex<Option<LoginInfo>>,
        observers: Mutex<Vec<Weak<dyn AccountChangeObserver>>>,
    }
    impl Account {
        fn switch_to(&self, email: Option<&str>) {
            *self.user.lock().unwrap() = email.map(|email| LoginInfo {
                email: email.into(),
                org_id: "test-org".into(),
            });
            let observers: Vec<_> = self
                .observers
                .lock()
                .unwrap()
                .iter()
                .filter_map(Weak::upgrade)
                .collect();
            for observer in observers {
                observer.account_changed();
            }
        }
    }
    #[async_trait]
    impl AuthHandle for Account {
        fn register_account_change_observer(&self, observer: Weak<dyn AccountChangeObserver>) {
            self.observers.lock().unwrap().push(observer);
        }
        async fn login(&self) -> Result<LoginInfo, AuthError> {
            self.current_user().await.ok_or(AuthError::Cancelled)
        }
        async fn logout(&self) -> Result<(), AuthError> {
            self.switch_to(None);
            Ok(())
        }
        async fn current_user(&self) -> Option<LoginInfo> {
            self.user.lock().unwrap().clone()
        }
    }

    #[derive(Default)]
    struct Files(AtomicUsize);
    #[async_trait]
    impl MemoryHierarchyProvider for Files {
        async fn load(&self, _cwd: &Path) -> Vec<MemoryFile> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        }
        async fn load_conditional_rules(
            &self,
            _cwd: &Path,
            _trigger: &Path,
            _mode: orchestrator::prompt::memory_block::InstructionFilesMode,
        ) -> Vec<MemoryFile> {
            Vec::new()
        }
    }

    fn root(
        identity: Arc<InstructionIdentity>,
        files: Arc<Files>,
    ) -> (Arc<ConversationOrchestrator>, Arc<MockApiClient>) {
        let api = Arc::new(MockApiClient::new(
            (0..4)
                .map(|_| mock_message_response(Vec::new(), Some("end_turn")))
                .collect(),
        ));
        let config = OrchestratorConfig {
            user_email: Some("stale-config@example.test".into()),
            ..Default::default()
        };
        let root = Arc::new(
            ConversationOrchestrator::new(
                config,
                api.clone(),
                Arc::new(tool_api::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                files,
                std::env::temp_dir(),
            )
            .with_instruction_user_email_provider(identity.clone()),
        );
        identity.bind_root(&root);
        (root, api)
    }

    async fn context(api: &MockApiClient) -> Option<String> {
        let requests = api.captured_msgs().await;
        let messages = requests.last().unwrap();
        assert!(messages
            .iter()
            .map(lingxi_core::types::ConversationMessage::text_content)
            .any(|text| text.contains("Today's date is ")));
        messages
            .iter()
            .rev()
            .map(lingxi_core::types::ConversationMessage::text_content)
            .find(|text| {
                text.contains("# userEmail") || text.contains("The session context was re-read ")
            })
    }

    #[tokio::test]
    async fn account_context_changes_email_and_omits_logout_without_clearing_files() {
        let account = Arc::new(Account::default());
        let identity = InstructionIdentity::new(account.clone());
        let files = Arc::new(Files::default());
        let (root, api) = root(identity, files.clone());
        root.run_turn("first").await.unwrap();
        assert!(context(&api).await.is_none());
        assert!(!serde_json::to_string(&api.captured_msgs().await)
            .unwrap()
            .contains("stale-config@example.test"));
        let original_loads = files.0.load(Ordering::SeqCst);
        account.switch_to(Some("  current@example.test  "));
        root.run_turn("second").await.unwrap();
        assert!(context(&api)
            .await
            .expect("account change reaches the model as session context")
            .contains("The user's email address is   current@example.test  ."));
        assert_eq!(
            files.0.load(Ordering::SeqCst),
            original_loads,
            "account pT retains eager file cache"
        );
        account.logout().await.unwrap();
        root.run_turn("third").await.unwrap();
        assert_eq!(
            context(&api).await.as_deref(),
            Some("<system-reminder>\nThe session context was re-read after the account changed; the values announced earlier (account, project, git status) no longer apply.\n</system-reminder>"),
            "logout sends the native correction after the prior account announcement"
        );
        let provider = orchestrator::prompt::memory_block::RootInstructionContextProvider::new();
        provider.bind(&root).unwrap();
        assert!(
            !lingxi_core::host::instructions::InstructionContextProvider::load(
                &provider,
                Path::new("/"),
                lingxi_core::host::instructions::InstructionScope::Full,
            )
            .await
            .unwrap()
            .user_context
            .contains_key("userEmail")
        );
        assert!(!serde_json::to_string(&api.captured_msgs().await)
            .unwrap()
            .contains("stale-config@example.test"));
        assert_eq!(files.0.load(Ordering::SeqCst), original_loads);
    }

    #[tokio::test]
    async fn account_observer_binding_does_not_keep_context_root_alive() {
        let account = Arc::new(Account::default());
        let identity = InstructionIdentity::new(account.clone());
        let weak_identity = Arc::downgrade(&identity);
        let (root, _) = root(identity, Arc::new(Files::default()));
        let weak_root = Arc::downgrade(&root);
        root.run_turn("load").await.unwrap();
        drop(root);
        assert!(weak_root.upgrade().is_none());
        assert!(weak_identity.upgrade().is_none());
        account.switch_to(Some("later@example.test"));
        assert!(account
            .observers
            .lock()
            .unwrap()
            .iter()
            .all(|observer| observer.upgrade().is_none()));
    }
}
