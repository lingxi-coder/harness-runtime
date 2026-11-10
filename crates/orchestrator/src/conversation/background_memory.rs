//! Main-session wiring for native file extraction and automatic consolidation.
use super::reminders_impl::{source_user_meta, wrap_system_reminder_source};
use super::*;
use lingxi_core::host::subagent_spawn::SubagentStopScope;
use lingxi_core::host::SubagentSpawnRequest;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use tasks::auto_dream::{AutoDreamRunContext, AutoDreamScheduler};

/// One admitted settings snapshot for the active local-memory store.
#[derive(Clone, Debug)]
pub struct MemoryPreferences {
    /// Validated directory, selected with the native source/trust precedence.
    pub directory: std::path::PathBuf,
    /// Explicit auto-memory setting before environment overrides.
    pub enabled: Option<bool>,
    /// Explicit consolidation preference before feature availability gates.
    pub auto_dream_enabled: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use lingxi_core::host::{BackgroundTaskHandle, RuntimeError, RuntimeSpawner};
    use std::path::PathBuf;

    struct NoBackgroundRuntime;
    #[async_trait::async_trait]
    impl RuntimeSpawner for NoBackgroundRuntime {
        async fn spawn(
            &self,
            _: &str,
            _: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
        ) -> Result<BackgroundTaskHandle, RuntimeError> {
            panic!("activation and context loading must not launch a memory selector");
        }
        async fn sleep(&self, _: std::time::Duration) {}
        async fn cancel(&self, _: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
            Ok(())
        }
    }
    struct NoQueries;
    #[async_trait::async_trait]
    impl sidequery::SideQueryClient for NoQueries {
        async fn query(
            &self,
            _: sidequery::SideQueryRequest,
        ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
            panic!("activation and index loading must not issue a selector request");
        }
    }
    fn orchestrator(prefetch: Arc<memory::prefetch::MemoryPrefetch>) -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
        .with_memory_prefetch(prefetch)
    }

    #[tokio::test]
    async fn recorded_memory_prompt_tracks_live_enable_disable_and_retarget_on_resume() {
        for exclude in [false, true] {
            check_recorded_memory_prompt_transitions(exclude).await;
        }
    }

    async fn check_recorded_memory_prompt_transitions(exclude: bool) {
        use lingxi_core::host::{FileSystem, PromptToolDescription};
        use platform_posix::fs::PosixFileSystem;
        use session::jsonl::writer::JsonlWriter;
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let mut roots = memory::memdir::paths::memdir_roots_at(temp.path(), temp.path(), false);
        roots.user_memdir = first.clone();
        let prefetch = Arc::new(memory::prefetch::MemoryPrefetch::new(
            Arc::new(memory::selector::MemorySelector::new(Arc::new(NoQueries))),
            Arc::new(NoBackgroundRuntime),
            roots,
        ));
        prefetch.set_enabled(false);
        let preferences = Arc::new(std::sync::Mutex::new(MemoryPreferences {
            directory: first.clone(),
            enabled: Some(false),
            auto_dream_enabled: None,
        }));
        let state = preferences.clone();
        let provider: Arc<MemoryPreferencesProvider> = Arc::new(move |_| {
            let value = state.lock().unwrap().clone();
            Box::pin(async move { Ok(value) })
        });
        let transcript = temp.path().join("memory-snapshots.jsonl");
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_owned()));
        let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
        let mut orch = orchestrator(prefetch.clone())
            .with_cache_safe_slot(slot)
            .with_memory_preferences_provider(provider)
            .with_jsonl_writer(Arc::new(JsonlWriter::new(transcript.clone(), fs.clone())));
        orch.config.exclude_dynamic_system_prompt_sections = exclude;
        orch.config.context_rendering =
            lingxi_core::host::instructions::InstructionRendering::Inline;
        orch.session.lock().await.model = "gpt-4o".into();
        let initial = orch
            .provider_system_prompt_for_main_request()
            .await
            .unwrap();
        orch.record_prompt_snapshot_if_needed(Some(&initial), &[])
            .await;
        let mut previous_inline = orch.additional_context_message().await.unwrap();
        orch.save_cache_safe_params_for_request(
            Some(&initial),
            "gpt-4o",
            &[],
            &[],
            previous_inline.as_ref(),
        )
        .await
        .unwrap();
        let mut previous = initial;
        let mut snapshot_a = None;
        let tools = vec![PromptToolDescription {
            name: "Read".into(),
            description: Utf16JsonProjection::parse(r#""frozen \ud800 description""#).unwrap(),
        }];
        orch.prompt_runtime
            .prompt_snapshot
            .lock()
            .await
            .as_mut()
            .unwrap()
            .tools = tools.clone();
        // Existing resumed snapshots may change memory state, while the
        // established missing-snapshot resume behavior remains untouched.
        orch.prompt_runtime
            .prompt_snapshot_resume
            .store(true, std::sync::atomic::Ordering::Release);
        for (directory, enabled) in [
            (first.clone(), true),
            (second.clone(), true),
            (second.clone(), false),
        ] {
            *preferences.lock().unwrap() = MemoryPreferences {
                directory: directory.clone(),
                enabled: Some(enabled),
                auto_dream_enabled: None,
            };
            let next = orch.provider_system_prompt().await.unwrap();
            let text = next.display_text();
            assert!(
                orch.prompt_runtime
                    .memory_prompt_dirty
                    .load(std::sync::atomic::Ordering::Acquire),
                "a render/preview cannot release old wire state"
            );
            orch.save_cache_safe_params_for_request(
                Some(&previous),
                "gpt-4o",
                &[],
                &[],
                previous_inline.as_ref(),
            )
            .await
            .unwrap();
            assert!(
                orch.prompt_runtime
                    .memory_prompt_dirty
                    .load(std::sync::atomic::Ordering::Acquire),
                "an older in-flight reply cannot publish the new memory prefix"
            );
            let next = orch
                .provider_system_prompt_for_main_request()
                .await
                .unwrap();
            let next_inline = orch.additional_context_message().await.unwrap();
            // A prepared request may be dropped before dispatch. A later
            // compaction save still carries the previous physical prefix.
            orch.save_cache_safe_params(Some(&previous), "gpt-4o", &[])
                .await
                .unwrap();
            assert!(
                orch.prompt_runtime
                    .memory_prompt_dirty
                    .load(std::sync::atomic::Ordering::Acquire),
                "compaction cannot acknowledge a prepared main prompt"
            );
            orch.save_cache_safe_params_for_request(
                Some(&next),
                "gpt-4o",
                &[],
                &[],
                next_inline.as_ref(),
            )
            .await
            .unwrap();
            assert!(!orch
                .prompt_runtime
                .memory_prompt_dirty
                .load(std::sync::atomic::Ordering::Acquire));
            if enabled {
                let policy = orch.current_prompt_policy().await.unwrap();
                let lingxi_core::host::prompt_policy::MemoryPromptBinding::Template(template) =
                    policy.memory
                else {
                    panic!("gpt-4o uses the generic memory protocol")
                };
                assert!(
                    text.contains(&template.replace("{memory_path}", &directory.to_string_lossy()))
                );
            } else {
                assert!(!text.contains("# Memory\nThe host provides persistent memory files"));
            }
            if directory == second {
                assert!(!text.contains(&first.to_string_lossy().to_string()));
            }
            let snapshot = orch
                .prompt_runtime
                .prompt_snapshot
                .lock()
                .await
                .clone()
                .unwrap();
            if directory == first && enabled {
                snapshot_a = Some(snapshot.clone());
            }
            assert_eq!(snapshot.tools, tools);
            assert!(orch
                .prompt_runtime
                .pending_memory_prompt_snapshot
                .lock()
                .await
                .is_none());
            let repeated = orch.provider_system_prompt().await.unwrap();
            assert_eq!(repeated.display_text(), text);
            previous = next;
            previous_inline = next_inline;
        }
        let rows = session::jsonl::reader::JsonlReader::new(transcript, fs)
            .read_all()
            .await
            .unwrap();
        let cold = crate::resume::prompt_snapshot_from_messages(&rows).unwrap();
        assert_eq!(cold.tools, tools);
        assert!(!cold
            .system_prompt
            .join("\n")
            .contains("# Memory\nThe host provides persistent memory files"));
        *orch.prompt_runtime.prompt_snapshot.lock().await = None;
        preferences.lock().unwrap().enabled = Some(true);
        let next = orch.provider_system_prompt().await.unwrap();
        orch.record_prompt_snapshot_if_needed(Some(&next), &[])
            .await;
        assert!(orch.prompt_runtime.prompt_snapshot.lock().await.is_none());
        assert!(orch
            .prompt_runtime
            .memory_prompt_dirty
            .load(std::sync::atomic::Ordering::Acquire));
        let next = orch
            .provider_system_prompt_for_main_request()
            .await
            .unwrap();
        let inline = orch.additional_context_message().await.unwrap();
        orch.save_cache_safe_params_for_request(Some(&next), "gpt-4o", &[], &[], inline.as_ref())
            .await
            .unwrap();
        assert!(!orch
            .prompt_runtime
            .memory_prompt_dirty
            .load(std::sync::atomic::Ordering::Acquire));

        // Offline settings changes are already reflected in composition;
        // only the restored snapshot still points at the old store.
        let snapshot_a = snapshot_a.unwrap();
        for enabled in [true, false] {
            let mut roots = memory::memdir::paths::memdir_roots_at(temp.path(), temp.path(), false);
            roots.user_memdir = second.clone();
            let prefetch = Arc::new(memory::prefetch::MemoryPrefetch::new(
                Arc::new(memory::selector::MemorySelector::new(Arc::new(NoQueries))),
                Arc::new(NoBackgroundRuntime),
                roots,
            ));
            prefetch.set_enabled(enabled);
            let directory = second.clone();
            let provider: Arc<MemoryPreferencesProvider> = Arc::new(move |_| {
                let directory = directory.clone();
                Box::pin(async move {
                    Ok(MemoryPreferences {
                        directory,
                        enabled: Some(enabled),
                        auto_dream_enabled: None,
                    })
                })
            });
            let mut cold = orchestrator(prefetch).with_memory_preferences_provider(provider);
            cold.config.exclude_dynamic_system_prompt_sections = exclude;
            cold.session.lock().await.model = "gpt-4o".into();
            *cold.prompt_runtime.prompt_snapshot.lock().await = Some(snapshot_a.clone());
            cold.prompt_runtime
                .prompt_snapshot_resume
                .store(true, std::sync::atomic::Ordering::Release);
            let rebuilt = cold.provider_system_prompt().await.unwrap().display_text();
            assert!(!rebuilt.contains(&first.to_string_lossy().to_string()));
            assert_eq!(
                rebuilt.contains("# Memory\nThe host provides persistent memory files"),
                enabled
            );
            assert_eq!(rebuilt.contains(second.to_string_lossy().as_ref()), enabled);
            let context = cold
                .additional_context_message()
                .await
                .unwrap()
                .unwrap()
                .text_content();
            assert!(!context.contains(&first.to_string_lossy().to_string()));
            assert!(!context.contains(second.to_string_lossy().as_ref()));
            assert_eq!(
                cold.prompt_runtime
                    .prompt_snapshot
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .tools,
                tools
            );
        }
    }

    #[tokio::test]
    async fn excluded_memory_directory_reaches_inline_announced_and_fork_context() {
        use lingxi_core::host::instructions::InstructionRendering;
        let temp = tempfile::tempdir().unwrap();
        for rendering in [
            InstructionRendering::Inline,
            InstructionRendering::Announced,
        ] {
            let mut systems = Vec::new();
            for name in ["first", "second"] {
                let directory = temp.path().join(name);
                let mut roots =
                    memory::memdir::paths::memdir_roots_at(temp.path(), temp.path(), false);
                roots.user_memdir = directory.clone();
                let prefetch = Arc::new(memory::prefetch::MemoryPrefetch::new(
                    Arc::new(memory::selector::MemorySelector::new(Arc::new(NoQueries))),
                    Arc::new(NoBackgroundRuntime),
                    roots,
                ));
                let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
                let mut orch = orchestrator(prefetch).with_cache_safe_slot(slot);
                orch.config.exclude_dynamic_system_prompt_sections = true;
                orch.config.context_rendering = rendering;
                orch.session.lock().await.model = "claude-sonnet-4-6".into();
                let system = orch
                    .provider_system_prompt_for_main_request()
                    .await
                    .unwrap();
                assert!(system
                    .display_text()
                    .contains(&crate::prompt::memory_section::render_auto_static()));
                assert!(!system
                    .display_text()
                    .contains(directory.to_string_lossy().as_ref()));
                systems.push(system.display_text());
                let expected = crate::prompt::memory_section::render_auto_directory(
                    &directory.to_string_lossy(),
                );
                let scalar = orch.additional_context_message().await.unwrap();
                let mut messages = vec![ConversationMessage::user(
                    MessageId::new(),
                    "request".into(),
                )];
                orch.session.lock().await.history = messages.clone();
                orch.project_dynamic_memory_context(&mut messages).await;
                let row = messages
                    .iter()
                    .find(|message| message.text_content().contains(&expected))
                    .unwrap()
                    .clone();
                assert_eq!(
                    messages
                        .iter()
                        .map(|message| message.text_content())
                        .collect::<String>()
                        .matches(directory.to_string_lossy().as_ref())
                        .count(),
                    1
                );
                assert!(orch
                    .instruction_context_snapshot()
                    .await
                    .user_context_utf8("auto memory")
                    .is_none());
                orch.save_cache_safe_params_for_request(
                    Some(&system),
                    "claude-sonnet-4-6",
                    &[],
                    &messages,
                    scalar.as_ref(),
                )
                .await
                .unwrap();
                let (fork, _, _, _) = orch.background_memory_snapshot().await.unwrap();
                let prefix = fork.fork_context_messages.unwrap();
                assert_eq!(
                    prefix
                        .iter()
                        .filter(|message| message.id() == row.id())
                        .count(),
                    1
                );
                assert_eq!(
                    prefix
                        .iter()
                        .map(|message| message.text_content())
                        .collect::<String>()
                        .matches(directory.to_string_lossy().as_ref())
                        .count(),
                    1
                );
                assert_eq!(fork.fork_prompt_policy.unwrap().bundle_id, "claude-code");
            }
            assert_eq!(systems[0], systems[1]);
        }
    }

    #[tokio::test]
    async fn live_activation_and_retarget_refresh_the_eager_index_without_touching_the_old_store() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let mut roots = memory::memdir::paths::memdir_roots_at(temp.path(), temp.path(), false);
        roots.user_memdir = first.clone();
        let prefetch = Arc::new(memory::prefetch::MemoryPrefetch::new(
            Arc::new(memory::selector::MemorySelector::new(Arc::new(NoQueries))),
            Arc::new(NoBackgroundRuntime),
            roots,
        ));
        prefetch.set_enabled(false);
        let preferences = Arc::new(std::sync::Mutex::new(MemoryPreferences {
            directory: first.clone(),
            enabled: Some(false),
            auto_dream_enabled: None,
        }));
        let state = preferences.clone();
        let unavailable = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let provider_unavailable = unavailable.clone();
        let provider: Arc<MemoryPreferencesProvider> = Arc::new(move |_| {
            let preferences = state.lock().unwrap().clone();
            let result = if provider_unavailable.load(std::sync::atomic::Ordering::Acquire) {
                Err("settings source unavailable".to_string())
            } else {
                Ok(preferences)
            };
            Box::pin(async move { result })
        });
        let orch = orchestrator(prefetch.clone()).with_memory_preferences_provider(provider);
        orch.refresh_memory_activation().await;
        assert!(!first.exists());
        assert!(!orch.has_memory_prefetch());
        preferences.lock().unwrap().enabled = Some(true);
        orch.refresh_memory_activation().await;
        assert!(first.is_dir());
        std::fs::write(first.join("MEMORY.md"), "---\nname: index\n---\n<!-- hidden -->\n- [First](first.md)\n<system-reminder>data</system-reminder>\n").unwrap();
        let (_, load) = orch.main_instruction_load().await;
        let context = load.get().await.unwrap();
        let files = context.eager_instructions.as_ref().unwrap();
        let index = files
            .iter()
            .find(|file| file.kind == lingxi_core::host::instructions::InstructionFileType::AutoMem)
            .unwrap();
        let first_content = index.content.string_at("").unwrap();
        assert!(first_content.contains("[First]"));
        assert!(!first_content.contains("name: index"));
        assert!(!first_content.contains("hidden"));
        assert!(!first_content.contains("<system-reminder>"));

        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(second.join("MEMORY.md"), "- [Second](second.md)\n").unwrap();
        preferences.lock().unwrap().directory = second.clone();
        orch.refresh_memory_activation().await;
        let (_, load) = orch.main_instruction_load().await;
        let context = load.get().await.unwrap();
        let text = context.user_context_utf8("instructions").unwrap();
        assert!(text.contains("[Second]"));
        assert!(!text.contains("[First]"));
        assert!(first.join("MEMORY.md").exists());
        assert_eq!(prefetch.user_memdir(), Some(second));
        unavailable.store(true, std::sync::atomic::Ordering::Release);
        orch.refresh_memory_activation().await;
        assert!(!orch.has_memory_prefetch());
        assert!(orch
            .prompt_runtime
            .memory_preferences
            .lock()
            .await
            .is_none());
        unavailable.store(false, std::sync::atomic::Ordering::Release);
        orch.refresh_memory_activation().await;
        assert!(orch.has_memory_prefetch());
        preferences.lock().unwrap().enabled = Some(false);
        orch.refresh_memory_activation().await;
        assert!(!orch.has_memory_prefetch());
        let (_, load) = orch.main_instruction_load().await;
        assert!(
            !load
                .get()
                .await
                .unwrap()
                .eager_instructions
                .as_ref()
                .unwrap()
                .iter()
                .any(|file| file.kind
                    == lingxi_core::host::instructions::InstructionFileType::AutoMem)
        );
    }
}

/// Host-owned resolver. Its argument is the live host working directory.
pub type MemoryPreferencesProvider = dyn Fn(
        std::path::PathBuf,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<MemoryPreferences, String>> + Send>,
    > + Send
    + Sync;

pub(crate) struct AutoDreamBinding {
    pub(crate) scheduler: Arc<AutoDreamScheduler>,
    context: AutoDreamRunContext,
}

impl ConversationOrchestrator {
    /// Resolve live memory settings using the host's admitted source layers.
    pub fn with_memory_preferences_provider(
        mut self,
        provider: Arc<MemoryPreferencesProvider>,
    ) -> Self {
        self.prompt_runtime.memory_preferences_provider = Some(provider);
        self
    }
    /// Refresh the admitted memory switch before assembling an outer prompt.
    /// Dormant stores retain their configured root without reading or creating it.
    pub(crate) async fn refresh_memory_activation(&self) {
        let _lifecycle = self.prompt_runtime.background_memory_lifecycle.lock().await;
        let Some(prefetch) = &self.prompt_runtime.memory_prefetch else {
            return;
        };
        let Some(mut directory) = prefetch.configured_user_memdir() else {
            return;
        };
        let mut retargeted = false;
        let was_active = prefetch.is_enabled();
        let mut settings_available = true;
        let setting = if let Some(provider) = &self.prompt_runtime.memory_preferences_provider {
            match provider(self.prompt_probe_cwd(&self.session_cwd.cwd())).await {
                Ok(preferences) => {
                    if preferences.directory != directory {
                        self.retire_memory_prompt_snapshot().await;
                        prefetch.set_enabled(false);
                        *self.prompt_runtime.pending_memory_prefetch.lock().await = None;
                        if let Some(handle) = &self.prompt_runtime.auto_memory {
                            handle.retarget(preferences.directory.clone()).await;
                        }
                        if let Some(binding) = &self.prompt_runtime.auto_dream {
                            binding.scheduler.shutdown().await;
                            let _ = binding.scheduler.drain_updates().await;
                        }
                        directory = preferences.directory.clone();
                        prefetch.retarget_user_memdir(directory.clone());
                        retargeted = true;
                    }
                    let enabled = preferences.enabled;
                    *self.prompt_runtime.memory_preferences.lock().await = Some(preferences);
                    enabled
                }
                Err(error) => {
                    tracing::debug!(%error, "memory preferences unavailable");
                    settings_available = false;
                    *self.prompt_runtime.memory_preferences.lock().await = None;
                    Some(false)
                }
            }
        } else if let Some(reader) = &self.mod_settings_reader {
            match reader
                .read(Utf16JsonProjection::plain(serde_json::json!({})))
                .await
            {
                Ok(settings) => settings
                    .at_pointer("/autoMemoryEnabled")
                    .and_then(|value| value.as_bool()),
                Err(error) => {
                    tracing::debug!(%error, "memory activation settings unavailable");
                    settings_available = false;
                    Some(false)
                }
            }
        } else {
            Some(prefetch.is_enabled())
        };
        let mut enabled = settings_available
            && memory::auto_memory_enabled(&memory::AutoMemoryEnv::from_process_env(), setting);
        if enabled {
            if let Err(error) = tokio::fs::create_dir_all(&directory).await {
                tracing::warn!(%error, "could not prepare memory directory");
                enabled = false;
            }
        }
        if retargeted || enabled != prefetch.is_enabled() {
            self.retire_memory_prompt_snapshot().await;
            prefetch.set_enabled(enabled);
            *self.prompt_runtime.pending_memory_prefetch.lock().await = None;
            self.refresh_instruction_context(if retargeted {
                lingxi_core::host::instructions::InstructionRefreshReason::SettingsSync
            } else if enabled {
                lingxi_core::host::instructions::InstructionRefreshReason::AutoMemoryBackOn
            } else {
                lingxi_core::host::instructions::InstructionRefreshReason::AutoMemoryOff
            });
        }
        if retargeted || (was_active && !enabled) {
            self.compaction_runtime.reset_session_memory().await;
        }
        // Composition has current settings before a cold resume mounts its
        // historical snapshot, so there may be no live root transition.
        if self.prompt_snapshot_resume()
            && !self
                .prompt_runtime
                .memory_resume_prompt_checked
                .load(std::sync::atomic::Ordering::Acquire)
        {
            let snapshot = self.prompt_runtime.prompt_snapshot.lock().await.clone();
            let custom_agent = self
                .lifecycle_runtime
                .main_thread_agent
                .read()
                .await
                .as_ref()
                .is_some_and(|agent| agent.system_prompt.is_some());
            let policy = self.current_prompt_policy().await.ok();
            if self.config.system_prompt_override.is_none()
                && !custom_agent
                && snapshot.as_ref().is_some_and(|snapshot| {
                    policy.as_ref().is_none_or(|policy| {
                        !crate::prompt::memory_section::snapshot_matches_directory(
                            snapshot,
                            enabled.then_some(directory.as_path()),
                            self.config.exclude_dynamic_system_prompt_sections,
                            policy,
                        )
                    })
                })
            {
                self.retire_memory_prompt_snapshot().await;
            }
            self.prompt_runtime
                .memory_resume_prompt_checked
                .store(true, std::sync::atomic::Ordering::Release);
        }
        if !enabled {
            if let Some(handle) = &self.prompt_runtime.auto_memory {
                handle.cancel().await;
            }
            if let Some(binding) = &self.prompt_runtime.auto_dream {
                binding.scheduler.shutdown().await;
            }
        }
    }

    /// A preview cannot release admission. Only a matching committed main
    /// prefix makes a changed memory store available to background forks.
    pub(crate) async fn publish_memory_cache_safe_params(
        &self,
        slot: &sidequery::CacheSafeParamsSlot,
        params: sidequery::CacheSafeParams,
        main_request: bool,
    ) {
        let _lifecycle = self.prompt_runtime.background_memory_lifecycle.lock().await;
        // The main turn holds turn_gate through response capture. A later
        // epoch-owning main request cannot overlap the request saved here.
        let current = main_request
            && self
                .prompt_runtime
                .main_memory_prompt_generation
                .load(std::sync::atomic::Ordering::Acquire)
                == self
                    .prompt_runtime
                    .memory_prompt_generation
                    .load(std::sync::atomic::Ordering::Acquire);
        slot.save(params).await;
        if current {
            self.prompt_runtime
                .memory_prompt_dirty
                .store(false, std::sync::atomic::Ordering::Release);
        }
    }

    /// Attach the tool-executing native file-memory extractor.
    pub fn with_auto_memory(mut self, handle: Arc<super::auto_memory::AutoMemoryHandle>) -> Self {
        self.prompt_runtime.auto_memory = Some(handle);
        self
    }

    /// Attach the project-scoped automatic dream scheduler.
    pub fn with_auto_dream(
        mut self,
        scheduler: Arc<AutoDreamScheduler>,
        context: AutoDreamRunContext,
    ) -> Self {
        self.prompt_runtime.auto_dream = Some(AutoDreamBinding { scheduler, context });
        self
    }

    /// Cancel and join background memory work before the host drops a session.
    pub async fn shutdown_background_memory(&self) {
        let _lifecycle = self.prompt_runtime.background_memory_lifecycle.lock().await;
        if let Some(handle) = &self.prompt_runtime.auto_memory {
            handle.cancel().await;
        }
        if let Some(binding) = &self.prompt_runtime.auto_dream {
            binding.scheduler.shutdown().await;
        }
    }

    pub(crate) async fn reset_background_memory(&self) {
        let _lifecycle = self.prompt_runtime.background_memory_lifecycle.lock().await;
        if let Some(handle) = &self.prompt_runtime.auto_memory {
            handle.reset().await;
        }
        if let Some(binding) = &self.prompt_runtime.auto_dream {
            binding.scheduler.shutdown().await;
            let _ = binding.scheduler.drain_updates().await;
        }
    }

    /// Capture the current route, rendered prefix and instructions. The caller
    /// appends its directive once; a cold session never starts an unrelated fork.
    async fn background_memory_snapshot(
        &self,
    ) -> Option<(
        SubagentSpawnRequest,
        Vec<ConversationMessage>,
        Arc<agent::memory_fork::MemoryForkProjection>,
        Option<std::path::PathBuf>,
    )> {
        let mut params = self
            .model_runtime
            .cache_safe_slot
            .as_ref()?
            .get_last()
            .await?;
        let (session_id, model, profile, history, tool_invoker) = {
            let session = self.session.lock().await;
            (
                session.session_id,
                session.model.clone(),
                session.model_profile.clone(),
                session.model_context_history(),
                self.prompt_runtime
                    .auto_memory
                    .as_ref()
                    .and_then(|handle| handle.snapshot_tool_invoker()),
            )
        };
        // Live history excludes request-transient context. Preserve the exact
        // route-specific memory row from the committed physical request when
        // extending its history, including its durable message identity.
        let dynamic_memory = params.fork_context_messages.iter().find(|message| {
            matches!(message, ConversationMessage::User { is_meta: true, origin: Some(origin), .. }
                if origin.string_at("/type") == Some("dynamic_memory_context"))
        }).cloned();
        extend_session_memory_fork_context(&mut params.fork_context_messages, &history);
        if dynamic_memory.is_some() {
            params.fork_context_messages.retain(|message| {
                !matches!(message, ConversationMessage::User { is_meta: true, origin: Some(origin), .. }
                    if origin.string_at("/type") == Some("dynamic_memory_context"))
            });
        }
        let transcript_directory = params
            .transcript_path
            .as_ref()
            .and_then(|path| path.parent())
            .map(std::path::Path::to_path_buf);
        let mut wire = agent::memory_fork::MemoryForkProjection::from_cache_params(&params);
        wire.tool_invoker = tool_invoker;
        let wire = Arc::new(wire);
        let mut prefix = Vec::new();
        if let Some(context) = params.user_context_message {
            prefix.push(context);
        }
        prefix.extend(params.fork_context_messages);
        if let Some(memory) = dynamic_memory {
            self.prepend_transient_leading_context(&mut prefix, memory);
        }
        let model = (!params.tool_use_options.main_loop_model.is_empty())
            .then(|| params.tool_use_options.main_loop_model.clone())
            .unwrap_or(model);
        let profile = if params.tool_use_options.main_loop_model.is_empty() {
            profile
        } else {
            params.tool_use_options.model_profile.clone()
        };
        let request = SubagentSpawnRequest {
            subagent_type: lingxi_core::host::fork_subagent::FORK_SUBAGENT_TYPE.into(),
            stop_hook_scope: SubagentStopScope::AgentScoped,
            origin_session_id: Some(session_id),
            cwd: Some(self.session_cwd.cwd().to_string_lossy().into_owned()),
            parent_model_override: Some(model),
            parent_model_profile_override: profile,
            parent_permission_mode: self.permission_mode(),
            fork_parent_system_prompt: Some((*params.system_prompt).clone()),
            fork_prompt_policy: params
                .tool_use_options
                .prompt_policy
                .as_ref()
                .map(|policy| policy.metadata()),
            fork_custom_system_prompt: self.config.system_prompt_override.clone(),
            fork_append_system_prompt: self.effective_append_system_prompt(),
            fork_role_system_prompt: self
                .lifecycle_runtime
                .main_thread_agent
                .read()
                .await
                .as_ref()
                .and_then(|agent| agent.system_prompt.clone()),
            fork_context_messages: Some(prefix),
            instruction_context: self.prompt_runtime.instruction_context.lock().await.clone(),
            effort: params.effort,
            ..Default::default()
        };
        Some((request, history, wire, transcript_directory))
    }

    async fn background_memory_writes_permitted(&self, directory: &std::path::Path) -> bool {
        use lingxi_core::host::permission_gate::{ModToolCheckDecision, PermissionCheckContext};
        // Native YGr inspects configured Write/Edit deny and ask rules without
        // prompting, executing a tool, or advancing the consolidation clock.
        let input = Utf16JsonProjection::plain(serde_json::json!({
            "file_path": directory.join("MEMORY.md").to_string_lossy(), "content": ""
        }));
        for name in ["Write", "Edit"] {
            if let Some(check) = self
                .perms
                .check_mod_query(
                    name,
                    &input,
                    &PermissionCheckContext {
                        mode_override: self.permission_mode(),
                        is_non_interactive_session: true,
                        background_owned: true,
                        ..Default::default()
                    },
                )
                .await
            {
                if check.verdict.rule.is_some()
                    && check.verdict.decision != ModToolCheckDecision::Allow
                {
                    return false;
                }
            }
        }
        true
    }

    /// Called once for a successful outer main turn, including streaming turns.
    pub(crate) async fn maybe_start_background_memory(&self) {
        // A settings change may have landed during a long foreground call.
        self.refresh_memory_activation().await;
        let _lifecycle = self.prompt_runtime.background_memory_lifecycle.lock().await;
        if self.config.bare || self.is_persistent_app_chat() {
            return;
        }
        // A change during the last foreground request invalidates its wire
        // prefix. Wait for the next main request to publish the new protocol
        // before a worker inherits that prefix with the new store.
        if self
            .prompt_runtime
            .memory_prompt_dirty
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        if self
            .prompt_runtime
            .memory_prefetch
            .as_ref()
            .is_some_and(|prefetch| !prefetch.is_enabled())
        {
            return;
        }
        let extract = self
            .prompt_runtime
            .auto_memory
            .as_ref()
            .filter(|_| telemetry::feature_flags::flag_bool("tengu_passport_quail", false));
        let dream_flag =
            telemetry::feature_flags::flag_value("tengu_onyx_plover", serde_json::Value::Null);
        let dream = self.prompt_runtime.auto_dream.as_ref();
        if extract.is_none()
            && dream.is_none_or(|_| {
                dream_flag
                    .get("enabled")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
                    && dream_flag
                        .get("available")
                        .and_then(serde_json::Value::as_bool)
                        != Some(true)
            })
        {
            return;
        }
        // Read only the host's admitted settings view; a read failure must not
        // enable writes that may have just been disabled by a policy change.
        let preferences = self.prompt_runtime.memory_preferences.lock().await.clone();
        let settings = if preferences.is_some() {
            None
        } else if let Some(reader) = &self.mod_settings_reader {
            match reader
                .read(Utf16JsonProjection::plain(serde_json::json!({})))
                .await
            {
                Ok(settings) => Some(settings),
                Err(error) => {
                    tracing::debug!(%error, "background memory settings unavailable");
                    return;
                }
            }
        } else {
            None
        };
        let enabled = preferences
            .as_ref()
            .and_then(|value| value.enabled)
            .or_else(|| {
                settings
                    .as_ref()
                    .and_then(|settings| settings.at_pointer("/autoMemoryEnabled"))
                    .and_then(|v| v.as_bool())
            });
        if !memory::auto_memory_enabled(&memory::AutoMemoryEnv::from_process_env(), enabled) {
            if let Some(handle) = &self.prompt_runtime.auto_memory {
                handle.cancel().await;
            }
            if let Some(binding) = dream {
                binding.scheduler.shutdown().await;
            }
            return;
        }
        let memory_directory = self
            .prompt_runtime
            .memory_prefetch
            .as_ref()
            .and_then(|prefetch| prefetch.user_memdir())
            .or_else(|| extract.map(|handle| handle.memory_dir()))
            .or_else(|| {
                dream.map(|binding| {
                    std::path::PathBuf::from(&binding.context.prompt_context.memory_directory)
                })
            });
        if let Some(directory) = memory_directory {
            if !self.background_memory_writes_permitted(&directory).await {
                return;
            }
        }
        let generation = extract.map(|handle| handle.generation());
        let Some((request, history, wire, transcript_directory)) =
            self.background_memory_snapshot().await
        else {
            return;
        };
        if let Some(handle) = extract {
            let cadence = telemetry::feature_flags::flag_value(
                "tengu_bramble_lintel",
                serde_json::Value::Null,
            )
            .as_f64()
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(1.0)
            .ceil();
            handle.set_every_n_turns(cadence.min(u32::MAX as f64) as u32);
            handle
                .submit(super::auto_memory::AutoMemorySnapshot {
                    request: request.clone(),
                    history,
                    wire: Some(wire.clone()),
                    generation: generation.expect("captured extractor generation"),
                })
                .await;
        }
        if let Some(binding) = dream {
            let mut context = binding.context.clone();
            context.current_session_id = request
                .origin_session_id
                .expect("captured session")
                .as_uuid()
                .to_string();
            context.fork_request = Some(Box::new(request));
            context.fork_wire = Some(wire);
            if let Some(directory) =
                transcript_directory.filter(|directory| !directory.as_os_str().is_empty())
            {
                context.prompt_context.transcript_directory =
                    memory::memdir::paths::memory_directory_prompt_path(&directory);
            }
            if let Some(directory) = self
                .prompt_runtime
                .memory_prefetch
                .as_ref()
                .and_then(|prefetch| prefetch.user_memdir())
            {
                context.prompt_context.memory_directory =
                    memory::memdir::paths::memory_directory_prompt_path(&directory);
            }
            context.gates = context.gates.with_flag(&dream_flag);
            // The outer turn has just admitted the live auto-memory switch.
            // The composition-time value may predate a settings change.
            context.gates.auto_memory_enabled = true;
            if let Some(preferences) = &preferences {
                context.gates.setting = preferences.auto_dream_enabled;
            } else if let Some(settings) = &settings {
                context.gates.setting = settings
                    .at_pointer("/autoDreamEnabled")
                    .and_then(|v| v.as_bool());
            }
            context.config = tasks::auto_dream::AutoDreamConfig::from_flag(&dream_flag);
            if let Err(error) = binding.scheduler.enqueue(context).await {
                tracing::debug!(%error, "could not schedule background consolidation");
            }
        }
    }

    pub(crate) async fn native_memory_update_reminders(&self) -> Vec<ConversationMessage> {
        use crate::prompt::memory_update::{
            render_memory_update, MemoryUpdate, MemoryUpdateSource,
        };
        let mut updates = Vec::new();
        if let Some(handle) = &self.prompt_runtime.auto_memory {
            updates.extend(
                handle
                    .drain_updates()
                    .into_iter()
                    .map(|update| (MemoryUpdateSource::Extract, update.summary, update.paths)),
            );
        }
        if let Some(binding) = &self.prompt_runtime.auto_dream {
            updates.extend(
                binding
                    .scheduler
                    .drain_updates()
                    .await
                    .into_iter()
                    .map(|update| (MemoryUpdateSource::Dream, update.summary, update.paths)),
            );
        }
        let index_path = self
            .prompt_runtime
            .memory_prefetch
            .as_ref()
            .and_then(|p| p.user_memdir())
            .map(|p| p.join("MEMORY.md"));
        let read_state = self
            .prompt_runtime
            .read_state_map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        updates
            .into_iter()
            .map(|(source, summary, paths)| {
                let in_context_paths = paths
                    .iter()
                    .filter(|path| read_state.contains(path) || index_path.as_ref() == Some(path))
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect();
                let body = render_memory_update(&MemoryUpdate {
                    source,
                    summary: summary.into(),
                    in_context_paths,
                    paths: paths
                        .into_iter()
                        .map(|path| path.to_string_lossy().into_owned())
                        .collect(),
                });
                source_user_meta(MessageId::new(), wrap_system_reminder_source(&body))
            })
            .collect()
    }
}
