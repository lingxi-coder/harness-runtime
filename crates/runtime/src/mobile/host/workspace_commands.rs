use std::path::{Path, PathBuf};

use client::protocol::events::ClientEvent;
use client::protocol::listings::{SessionRowDto, WorkspaceStateDto};
use lingxi_core::host::OrchestratorHandle;

use super::{ClientError, MobileEngineHandle};

#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum MobileSessionDiscoveryScope {
    Current,
    Repository,
    All,
}

pub(super) struct MobileWorkspaceCatalog {
    pub registry: std::sync::Arc<tokio::sync::RwLock<command_api::CommandRegistry>>,
    pub agents: std::sync::Arc<tokio::sync::RwLock<Vec<agent::AgentDefinition>>>,
    pub config_home: PathBuf,
    pub home: PathBuf,
    pub device_skill_tools: Vec<String>,
    pub media_tools: std::sync::Arc<tool_api::ToolRegistry>,
    pub workflow_tool: std::sync::Arc<tool_workflow::WorkflowTool>,
    pub workflow_commands: std::sync::Arc<crate::workflow_catalog::WorkflowCommandSnapshot>,
}

impl MobileWorkspaceCatalog {
    pub async fn reload(&self, cwd: &Path, roots: &[PathBuf]) -> Result<(), String> {
        use command_api::BuiltinCommandHandler;
        let staged =
            std::sync::Arc::new(tokio::sync::RwLock::new(self.registry.read().await.clone()));
        let handler = super::mobile_reload_skills_handler(
            staged.clone(),
            cwd.to_path_buf(),
            self.config_home.clone(),
            self.home.clone(),
            self.device_skill_tools.clone(),
            self.media_tools.clone(),
            self.workflow_tool.clone(),
            self.workflow_commands.clone(),
            roots.to_vec(),
        );
        let parsed = command_api::parse_slash_command(&"/reload-skills".into())
            .ok_or("invalid reload command")?;
        if let command_api::CommandResult::Failed { source } = handler.handle(&parsed).await {
            return Err(source.to_string());
        }
        let directories = agent::catalog::agent_dir_precedence(
            self.config_home.join("agents"),
            cwd,
            &self.home,
            permission::set_cwd::project_root_of(cwd).as_deref(),
            roots,
        );
        let loaded_agents = agent::load_agents_from_dirs(&directories).await;
        let mut replacement = staged.read().await.clone();
        replacement.register_builtin_handler(std::sync::Arc::new(
            super::mobile_reload_skills_handler(
                self.registry.clone(),
                cwd.to_path_buf(),
                self.config_home.clone(),
                self.home.clone(),
                self.device_skill_tools.clone(),
                self.media_tools.clone(),
                self.workflow_tool.clone(),
                self.workflow_commands.clone(),
                roots.to_vec(),
            ),
        ));
        replacement.register_builtin_handler(std::sync::Arc::new(
            command_api::builtins::SkillsHandler::with_all_roots(
                cwd.to_path_buf(),
                self.config_home.clone(),
                None,
                roots
                    .iter()
                    .map(|root| root.join(branding::DOT_DIR).join("skills"))
                    .collect(),
            ),
        ));
        let mut registry = self.registry.write().await;
        let mut agents = self.agents.write().await;
        agents.retain(|definition| {
            matches!(
                definition.source,
                agent::AgentSource::BuiltIn | agent::AgentSource::Plugin
            )
        });
        agent::catalog::merge_agents_later_wins(&mut agents, loaded_agents);
        *registry = replacement;
        Ok(())
    }
}

impl MobileEngineHandle {
    pub(super) fn current_mobile_config(&self) -> Result<super::MobileConfig, ClientError> {
        let mut cfg = self.firer_cfg.clone();
        let logical = PathBuf::from(self.session_cwd());
        cfg.cwd = self
            .fs
            .translate_model_path(&logical.to_string_lossy(), false)
            .map_err(|error| ClientError::Rejected {
                message: error.to_string(),
            })?
            .map(PathBuf::from)
            .unwrap_or_else(|| logical.clone());
        cfg.logical_cwd = Some(logical);
        Ok(cfg)
    }

    pub(super) fn current_settings(
        &self,
    ) -> Option<::configuration_admin::settings_bridge::SettingsContext> {
        let initial = self.settings.as_ref()?;
        Some(::configuration_admin::settings_bridge::SettingsContext {
            plugin_archive_session: initial.plugin_archive_session.clone(),
            fusion_layers: initial.fusion_layers.clone(),
            paths: ::configuration_admin::settings_bridge::SettingsPaths {
                lingxi_home: self.lingxi_home.clone(),
                project_dir: self.current_mobile_config().ok()?.cwd,
            },
            active: initial.active.clone(),
            managed: initial.managed.clone(),
            managed_diagnostics: initial.managed_diagnostics.clone(),
        })
    }

    fn workspace_state_snapshot(&self) -> WorkspaceStateDto {
        let path = self.inner.session_writer.active_path();
        let transcript_path = self
            .logical_path_for_host(&path)
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        WorkspaceStateDto {
            session_id: self.active_session_id(),
            cwd: self.session_cwd(),
            project_root: self
                .inner
                .session_cwd
                .project_root()
                .to_string_lossy()
                .into_owned(),
            additional_directories: self
                .inner
                .session_cwd
                .explicit_trusted_dirs()
                .iter()
                .filter_map(|path| path.to_str())
                .filter(|path| {
                    self.inner.mobile_linux.is_none()
                        || path.starts_with(mobile_linux_api::guest_paths::HOME)
                })
                .map(str::to_string)
                .collect(),
            transcript_path,
        }
    }

    fn check_workspace_owner(&self, expected_session_id: &str) -> Result<(), ClientError> {
        if expected_session_id
            .strip_prefix("sess:")
            .unwrap_or(expected_session_id)
            != self.active_session_id()
        {
            return Err(ClientError::Rejected {
                message:
                    "the session changed while choosing a directory; reopen the directory picker"
                        .into(),
            });
        }
        Ok(())
    }
    fn logical_path_for_host(&self, host: &Path) -> Option<String> {
        let runtime = self.inner.mobile_linux.as_ref()?;
        let mut mounts = runtime.current_mounts();
        for mount in &mut mounts {
            mount.host_path = PathBuf::from(super::canonical_cwd_string(&mount.host_path));
        }
        mobile_linux_api::map_host_path_to_guest(
            Path::new(&super::canonical_cwd_string(host)),
            &mounts,
        )
    }

    pub(super) fn workspace_target(&self, input: &str) -> Result<(PathBuf, PathBuf), ClientError> {
        let cwd = self.inner.session_cwd.cwd();
        let guest_home = self
            .inner
            .mobile_linux
            .as_ref()
            .map(|_| Path::new(mobile_linux_api::guest_paths::HOME));
        let requested = command_api::workspace_directory::resolve_path(input, &cwd, guest_home);
        let host = self
            .fs
            .translate_model_path(&requested.to_string_lossy(), false)
            .map_err(|error| ClientError::Rejected {
                message: error.to_string(),
            })?
            .map(PathBuf::from)
            .unwrap_or_else(|| requested.clone());
        let host = host.canonicalize().map_err(|error| ClientError::Rejected {
            message: format!(
                "Couldn't find a directory at {}: {error}",
                requested.display()
            ),
        })?;
        if !host.is_dir() {
            return Err(ClientError::Rejected {
                message: format!("{} is not a directory", requested.display()),
            });
        }
        let logical = if self.inner.mobile_linux.is_some() {
            let logical = self
                .logical_path_for_host(&host)
                .map(PathBuf::from)
                .ok_or_else(|| ClientError::Rejected {
                    message: "directory is outside the Linux user's mounted storage".into(),
                })?;
            if !logical.starts_with(mobile_linux_api::guest_paths::HOME) {
                return Err(ClientError::Rejected {
                    message: "workspace must remain inside the Linux user's home".into(),
                });
            }
            logical
        } else {
            host.clone()
        };
        Ok((logical, host))
    }

    pub(super) async fn change_workspace_inner(&self, input: &str) -> Result<(), ClientError> {
        if self.active_cancel.lock().await.is_some() || !self.message_queue.is_empty().await {
            return Err(ClientError::Rejected {
                message: "working directory can only change while the session is idle".into(),
            });
        }
        let (logical, host) = self.workspace_target(input)?;
        let (rules, roots) = self
            .inner
            .permission_policy_gate
            .directory_permission_snapshot();
        if let Some(roots) = roots {
            let requested = command_api::workspace_directory::resolve_path(
                input,
                &self.inner.session_cwd.cwd(),
                self.inner
                    .mobile_linux
                    .as_ref()
                    .map(|_| Path::new(mobile_linux_api::guest_paths::HOME)),
            );
            let check = command_api::workspace_directory::check_cd_permission_with_paths(
                &[requested, logical.clone(), host.clone()],
                &[logical.clone(), host.clone()],
                &rules,
                &roots,
            );
            if check != command_api::cd_permissions::CdCheck::Allowed {
                return Err(ClientError::Rejected {
                    message: command_api::cd_permissions::format_cd_rejection(
                        &logical.to_string_lossy(),
                        &check,
                        false,
                    ),
                });
            }
        }
        let mut trusted = self.inner.session_cwd.trusted_dirs();
        for path in [logical.clone(), host] {
            if !trusted.contains(&path) {
                trusted.push(path);
            }
        }
        self.inner
            .orchestrator
            .change_working_directory(logical, trusted)
            .await
            .map_err(|error| ClientError::Rejected {
                message: error.to_string(),
            })?;
        // The transcript/cwd transaction is committed. Publish that identity
        // even if reloading an optional project service subsequently fails.
        self.emit_workspace_state().await;
        if let Some(scheduler) = &self.session_cron {
            scheduler
                .set_tasks_file(cron::tasks_file::session_scheduled_tasks_path(
                    &self.current_mobile_config()?.cwd,
                ))
                .await
                .map_err(|error| ClientError::Internal {
                    message: error.to_string(),
                })?;
        }
        self.reload_workspace_catalog(None).await?;
        self.reload_configured_mcp().await;
        Ok(())
    }

    pub(super) async fn add_workspace_inner(&self, input: &str) -> Result<(), ClientError> {
        if self.active_cancel.lock().await.is_some() || !self.message_queue.is_empty().await {
            return Err(ClientError::Rejected {
                message: "working directories can only change while the session is idle".into(),
            });
        }
        let (logical, host) = self.workspace_target(input)?;
        let source = self.inner.session_writer.active_path();
        if let Some(bucket) = source.parent() {
            session::jsonl::record_session_alias(&self.lingxi_home, bucket, &logical)
                .await
                .map_err(|error| ClientError::Internal {
                    message: format!("persist directory association: {error}"),
                })?;
        }
        self.reload_workspace_catalog(Some(host.clone())).await?;
        self.inner.session_cwd.add_trusted_dir(logical.clone());
        self.inner.session_cwd.add_trusted_dir(host.clone());
        let update = serde_json::json!({"type":"addDirectories", "directories":[logical.to_string_lossy(), host.to_string_lossy()], "destination":"session"});
        if let Ok(update) =
            lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(&update)
        {
            self.inner
                .permission_policy_gate
                .apply_permission_update(&update);
        }
        self.inner.mcp_registry.add_root(logical.clone());
        self.inner
            .mcp_registry
            .notify_roots_list_changed_all()
            .await;
        self.inner
            .orchestrator
            .fire_directory_added(&logical.to_string_lossy(), "slash_command")
            .await;
        self.emit_workspace_state().await;
        Ok(())
    }

    pub(super) async fn reload_workspace_catalog(
        &self,
        additional: Option<PathBuf>,
    ) -> Result<(), ClientError> {
        let mut roots = self
            .inner
            .session_cwd
            .explicit_trusted_dirs()
            .iter()
            .filter_map(|path| {
                self.fs
                    .translate_model_path(&path.to_string_lossy(), false)
                    .ok()
                    .flatten()
                    .map(PathBuf::from)
                    .or_else(|| Some(path.clone()))
            })
            .collect::<Vec<_>>();
        if let Some(path) = additional {
            roots.push(path);
        }
        roots.sort();
        roots.dedup();
        self.inner
            .workspace_catalog
            .reload(&self.current_mobile_config()?.cwd, &roots)
            .await
            .map_err(|message| ClientError::Internal { message })?;
        self.inner.dispatcher.refresh_command_snapshot().await;
        Ok(())
    }

    pub(super) async fn workspace_sessions_inner(
        &self,
        scope: MobileSessionDiscoveryScope,
        limit: u32,
    ) -> Result<Vec<SessionRowDto>, ClientError> {
        let scope = match scope {
            MobileSessionDiscoveryScope::Current => session::jsonl::SessionDiscoveryScope::Current,
            MobileSessionDiscoveryScope::Repository => {
                session::jsonl::SessionDiscoveryScope::Repository
            }
            MobileSessionDiscoveryScope::All => session::jsonl::SessionDiscoveryScope::All,
        };
        let rows = if let Some(runtime) = &self.inner.mobile_linux {
            let mut roots = Vec::new();
            if scope == session::jsonl::SessionDiscoveryScope::Repository
                && self
                    .current_mobile_config()?
                    .cwd
                    .ancestors()
                    .any(|directory| directory.join(".git").exists())
            {
                if let Ok(output) = runtime
                    .run(mobile_linux_api::LinuxCommandRequest {
                        command: "git".into(),
                        args: vec!["worktree".into(), "list".into(), "--porcelain".into()],
                        cwd: Some(self.session_cwd()),
                        env: Default::default(),
                        stdin: None,
                        timeout_ms: Some(2000),
                        network: mobile_linux_api::NetworkPolicy::Allowed,
                        resource_limits: Default::default(),
                        mounts: Vec::new(),
                    })
                    .await
                {
                    if output.exit_code == 0 && !output.timed_out && !output.cancelled {
                        roots = output
                            .stdout
                            .lines()
                            .filter_map(|line| line.strip_prefix("worktree "))
                            .filter_map(|path| {
                                self.workspace_target(path)
                                    .ok()
                                    .map(|(logical, _)| logical.to_string_lossy().into_owned())
                            })
                            .collect();
                    }
                }
            }
            session::jsonl::list_recent_sessions_in_scope_with_worktrees(
                &self.lingxi_home,
                &self.session_cwd(),
                scope,
                limit as usize,
                self.fs.clone(),
                &roots,
            )
            .await
            .map(|catalog| catalog.sessions)
        } else {
            session::jsonl::list_recent_sessions_in_scope(
                &self.lingxi_home,
                &self.session_cwd(),
                scope,
                limit as usize,
                self.fs.clone(),
            )
            .await
        };
        let rows = match rows {
            Ok(rows) => rows,
            Err(session::jsonl::LoaderError::EmptyDirectory) => Vec::new(),
            Err(error) => {
                return Err(ClientError::Internal {
                    message: format!("session catalog: {error}"),
                });
            }
        };
        Ok(rows
            .iter()
            .map(|row| {
                let mut lowered = client::adapter::lowering::lower_session_metadata(row);
                lowered.path = self
                    .logical_path_for_host(&row.path)
                    .unwrap_or(lowered.path);
                lowered
            })
            .collect())
    }

    pub(super) async fn emit_workspace_state(&self) {
        self.event_sink
            .emit(ClientEvent::WorkspaceChanged {
                state: self.workspace_state_snapshot(),
            })
            .await;
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export(async_runtime = "tokio"))]
impl MobileEngineHandle {
    /// Read one complete directory/session generation, after any active
    /// identity transition has published its writer and cwd together.
    pub async fn workspace_state(&self) -> WorkspaceStateDto {
        let _transition = self.loop_transition.lock().await;
        self.workspace_state_snapshot()
    }

    pub async fn change_workspace(
        &self,
        path: String,
        expected_session_id: String,
    ) -> Result<(), ClientError> {
        let _transition = self.loop_transition.lock().await;
        self.check_workspace_owner(&expected_session_id)?;
        self.change_workspace_inner(&path).await
    }

    pub async fn add_workspace_directory(
        &self,
        path: String,
        expected_session_id: String,
    ) -> Result<(), ClientError> {
        let _transition = self.loop_transition.lock().await;
        self.check_workspace_owner(&expected_session_id)?;
        self.add_workspace_inner(&path).await
    }

    /// Resume the exact source selected in a Current/Repository/All catalog.
    pub async fn resume_workspace_session(
        &self,
        session_id: String,
        transcript_path: String,
    ) -> Result<(), ClientError> {
        let _transition = self.loop_transition.lock().await;
        let path = self
            .fs
            .translate_model_path(&transcript_path, false)
            .map_err(|error| ClientError::Rejected {
                message: error.to_string(),
            })?
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&transcript_path));
        let path = path.canonicalize().map_err(|error| ClientError::Rejected {
            message: format!("session source unavailable: {error}"),
        })?;
        let catalog = self
            .lingxi_home
            .join("projects")
            .canonicalize()
            .map_err(|error| ClientError::Rejected {
                message: error.to_string(),
            })?;
        let relative = path
            .strip_prefix(catalog)
            .map_err(|_| ClientError::Rejected {
                message: "selected transcript is outside the session catalog".into(),
            })?;
        let id = session_id.strip_prefix("sess:").unwrap_or(&session_id);
        if relative.components().count() != 2
            || path.extension().and_then(|value| value.to_str()) != Some("jsonl")
            || path.file_stem().and_then(|value| value.to_str()) != Some(id)
        {
            return Err(ClientError::Rejected {
                message: "selected transcript does not match the requested session UUID".into(),
            });
        }
        self.resume_session_impl(session_id, None, None, Some(path))
            .await
    }

    pub async fn workspace_sessions(
        &self,
        scope: MobileSessionDiscoveryScope,
        limit: u32,
    ) -> Result<Vec<SessionRowDto>, ClientError> {
        let _transition = self.loop_transition.lock().await;
        self.workspace_sessions_inner(scope, limit).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mobile::test_support::{
        new_engine_with_streaming, test_config, CollectingPermissionSink, FakeListener,
        HostFakePlatform,
    };
    use mobile_linux_api::MobileLinuxRuntime;
    use std::sync::Arc;

    fn fixture() -> (
        tempfile::TempDir,
        crate::mobile::MobileUserDirectories,
        super::super::MobileConfig,
        Arc<dyn lingxi_core::host::Platform>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime");
        let directories =
            crate::mobile::mobile_user_directories(root.display().to_string()).unwrap();
        directories.prepare().unwrap();
        let a = Path::new(&directories.projects).join("a");
        let b = Path::new(&directories.projects).join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let runtime = Arc::new(mobile_linux_android::AndroidProotRuntime::new(
            mobile_linux_android::AndroidProotRuntimeConfig {
                managed_root: root,
                brand_profile: mobile_linux_api::BrandRuntimeProfile::default(),
                app_sandbox_root: temp.path().to_path_buf(),
                abi: "arm64-v8a".into(),
                rootfs_version: "test".into(),
                archive_sha256: None,
                native_library_dir: None,
            },
        ));
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(runtime.configure_mounts(vec![
            mobile_linux_api::MountSpec {
                host_path: PathBuf::from(&directories.projects),
                guest_path: directories.projects_guest_path.clone(),
                read_only: false,
                purpose: mobile_linux_api::MountPurpose::Workspace,
            },
            // Linux CI's host fixture lives under /tmp, which is also fenced
            // guest scratch space. Explicitly back only this private fixture
            // tree so native session paths behave like Android/iOS app data;
            // unrelated guest scratch paths must remain inaccessible.
            mobile_linux_api::MountSpec {
                host_path: temp.path().to_path_buf(),
                guest_path: temp.path().to_string_lossy().into_owned(),
                read_only: false,
                purpose: mobile_linux_api::MountPurpose::Temp,
            },
        ]))
        .unwrap();
        drop(rt);
        let platform =
            Arc::new(HostFakePlatform::new(temp.path().to_path_buf()).with_mobile_linux(runtime))
                as Arc<dyn lingxi_core::host::Platform>;
        let mut cfg = test_config(&a);
        cfg.cwd = a;
        cfg.logical_cwd = Some(Path::new(&directories.projects_guest_path).join("a"));
        cfg.lingxi_home = PathBuf::from(&directories.config_home);
        (temp, directories, cfg, platform)
    }

    #[test]
    fn directory_move_and_cold_resume_share_guest_identity_and_exact_source() {
        let (_temp, directories, cfg, platform) = fixture();
        let handle = new_engine_with_streaming(
            cfg.clone(),
            platform.clone(),
            Arc::new(FakeListener::default()),
            Arc::new(CollectingPermissionSink::default()),
            None,
        )
        .unwrap();
        let id = handle.workspace_state_snapshot().session_id;
        let a = cfg.logical_cwd.as_ref().unwrap().display().to_string();
        let b = format!("{}/b", directories.projects_guest_path);
        let source = handle.inner.session_writer.active_path();
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        let message = serde_json::json!({"type":"user","sessionId":id,"uuid":uuid::Uuid::new_v4().to_string(),"parentUuid":null,"cwd":a,"isSidechain":false,"timestamp":"2026-10-22T12:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"original directory"}]}});
        std::fs::write(&source, format!("{message}\n")).unwrap();
        let moved = handle.runtime().block_on(async {
            handle
                .resume_workspace_session(id.clone(), source.display().to_string())
                .await
                .unwrap();
            handle
                .add_workspace_directory(b.clone(), id.clone())
                .await
                .unwrap();
            handle
                .change_workspace(b.clone(), id.clone())
                .await
                .unwrap();
            let state = handle.workspace_state_snapshot();
            assert_eq!(state.session_id, id);
            assert_eq!(state.cwd, b);
            assert_eq!(state.project_root, a);
            assert!(!source.exists());
            handle
                .fs
                .write_file(&format!("{b}/after-move.txt"), "written in B")
                .await
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(Path::new(&directories.projects).join("b/after-move.txt"))
                    .unwrap(),
                "written in B"
            );
            assert!(!Path::new(&directories.projects)
                .join("a/after-move.txt")
                .exists());
            let rows = handle
                .workspace_sessions(MobileSessionDiscoveryScope::Current, 100)
                .await
                .unwrap();
            assert!(rows
                .iter()
                .any(|row| row.uuid == id && row.path == state.transcript_path));
            state.transcript_path
        });
        drop(handle);
        let resumed = new_engine_with_streaming(
            cfg,
            platform,
            Arc::new(FakeListener::default()),
            Arc::new(CollectingPermissionSink::default()),
            None,
        )
        .unwrap();
        resumed
            .runtime()
            .block_on(resumed.resume_workspace_session(id.clone(), moved.clone()))
            .unwrap();
        let state = resumed.workspace_state_snapshot();
        assert_eq!(
            (
                state.session_id,
                state.cwd,
                state.project_root,
                state.transcript_path
            ),
            (id, b, a, moved)
        );
    }

    #[test]
    fn project_skill_documents_follow_the_native_backing_directory_after_cd() {
        let (_temp, directories, cfg, platform) = fixture();
        let skill = Path::new(&directories.projects).join("b/.lingxi/skills/current-project");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: current-project\ndescription: Current project fixture\n---\nDirectory B skill body\n",
        )
        .unwrap();
        let listener = Arc::new(FakeListener::default());
        let handle = new_engine_with_streaming(
            cfg,
            platform,
            listener.clone(),
            Arc::new(CollectingPermissionSink::default()),
            None,
        )
        .unwrap();
        handle.runtime().block_on(async {
            let owner = handle.workspace_state().await.session_id;
            handle
                .change_workspace(format!("{}/b", directories.projects_guest_path), owner)
                .await
                .unwrap();
            handle
                .submit(client::protocol::commands::ClientCommand::SkillAdmin {
                    command: client::protocol::commands::SkillAdminCommandDto {
                        action: "get_document".into(),
                        operation_id: None,
                        target: Some(skill.to_string_lossy().to_string().into()),
                        scope: None,
                        revision: None,
                        payload_json: None,
                    },
                })
                .await
                .unwrap();
            assert!(listener.received.lock().await.iter().any(|event| {
                matches!(event, ClientEvent::SkillDocument { document_json }
                    if document_json.contains("Directory B skill body"))
            }));
        });
    }

    #[test]
    fn missing_directory_and_wrong_selected_uuid_leave_owner_unchanged() {
        let (_temp, directories, cfg, platform) = fixture();
        let handle = new_engine_with_streaming(
            cfg,
            platform,
            Arc::new(FakeListener::default()),
            Arc::new(CollectingPermissionSink::default()),
            None,
        )
        .unwrap();
        let before = handle.workspace_state_snapshot();
        handle.runtime().block_on(async {
            assert!(handle
                .change_workspace(
                    format!("{}/missing", directories.projects_guest_path),
                    before.session_id.clone()
                )
                .await
                .is_err());
            assert!(handle
                .change_workspace("/root".into(), before.session_id.clone())
                .await
                .is_err());
            assert!(handle
                .resume_workspace_session(
                    uuid::Uuid::new_v4().to_string(),
                    before.transcript_path.clone()
                )
                .await
                .is_err());
        });
        assert_eq!(handle.workspace_state_snapshot(), before);
    }
    #[test]
    fn stale_picker_and_denied_cd_leave_session_and_transcript_unchanged() {
        let (_temp, directories, cfg, platform) = fixture();
        let settings = cfg.cwd.join(branding::DOT_DIR);
        std::fs::create_dir_all(&settings).unwrap();
        std::fs::write(
            settings.join("settings.json"),
            r#"{"permissions":{"deny":["Cd"]}}"#,
        )
        .unwrap();
        let handle = new_engine_with_streaming(
            cfg,
            platform,
            Arc::new(FakeListener::default()),
            Arc::new(CollectingPermissionSink::default()),
            None,
        )
        .unwrap();
        let before = handle.workspace_state_snapshot();
        handle.runtime().block_on(async {
            let b = format!("{}/b", directories.projects_guest_path);
            assert!(handle
                .add_workspace_directory(b.clone(), uuid::Uuid::new_v4().to_string())
                .await
                .is_err());
            let error = handle
                .change_workspace(b, before.session_id.clone())
                .await
                .unwrap_err();
            assert!(error.to_string().contains("Cd"), "{error}");
        });
        assert_eq!(handle.workspace_state_snapshot(), before);
    }

    #[test]
    fn linked_directory_reloads_skills_and_cron_store_follows_current_cwd() {
        let (_temp, directories, cfg, platform) = fixture();
        let skill = Path::new(&directories.projects)
            .join("b")
            .join(branding::DOT_DIR)
            .join("skills/workspace-skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: workspace-skill\ndescription: linked workspace regression\n---\nUse the linked source.").unwrap();
        let handle = new_engine_with_streaming(
            cfg,
            platform,
            Arc::new(FakeListener::default()),
            Arc::new(CollectingPermissionSink::default()),
            None,
        )
        .unwrap();
        let owner = handle.workspace_state_snapshot().session_id;
        handle.runtime().block_on(async {
            let b = format!("{}/b", directories.projects_guest_path);
            handle
                .add_workspace_directory(b.clone(), owner.clone())
                .await
                .unwrap();
            assert!(handle
                .inner
                .slash_registry
                .read()
                .await
                .list_all()
                .iter()
                .any(|command| command.name == "workspace-skill".into()));
            handle.change_workspace(b.clone(), owner).await.unwrap();
            let store = handle.cron_store().await.unwrap();
            store
                .create("0 9 * * *".into(), "local task".into(), true)
                .await
                .unwrap();
            assert!(Path::new(&directories.projects)
                .join("b")
                .join(branding::DOT_DIR)
                .join("scheduled_tasks.json")
                .is_file());
            assert!(!Path::new(&directories.projects)
                .join("a")
                .join(branding::DOT_DIR)
                .join("scheduled_tasks.json")
                .exists());
        });
    }
    #[test]
    fn empty_session_restores_its_exact_directory_from_another_workspace() {
        let (_temp, directories, cfg, platform) = fixture();
        let handle = new_engine_with_streaming(
            cfg,
            platform,
            Arc::new(FakeListener::default()),
            Arc::new(CollectingPermissionSink::default()),
            None,
        )
        .unwrap();
        handle.runtime().block_on(async {
            let id = uuid::Uuid::new_v4();
            let b = format!("{}/b", directories.projects_guest_path);
            handle
                .persist_mobile_empty_session_anchor(id, &b, "Empty in B")
                .await
                .unwrap();
            let path = session::jsonl::session_path(&handle.lingxi_home, &b, &id.to_string());
            let rows = handle
                .workspace_sessions(MobileSessionDiscoveryScope::All, 100)
                .await
                .unwrap();
            assert_eq!(
                rows.iter()
                    .find(|row| row.uuid == id.to_string())
                    .unwrap()
                    .workspace_path
                    .as_deref(),
                Some(b.as_str())
            );
            handle
                .resume_workspace_session(id.to_string(), path.display().to_string())
                .await
                .unwrap();
            let state = handle.workspace_state().await;
            assert_eq!(state.cwd, b);
            assert_eq!(state.project_root, b);
            assert_eq!(state.session_id, id.to_string());
        });
    }

    #[test]
    fn moving_an_empty_session_preserves_its_original_root_after_cold_resume() {
        let (_temp, directories, cfg, platform) = fixture();
        let handle = new_engine_with_streaming(
            cfg.clone(),
            platform.clone(),
            Arc::new(FakeListener::default()),
            Arc::new(CollectingPermissionSink::default()),
            None,
        )
        .unwrap();
        let (id, path, original, b) = handle.runtime().block_on(async {
            handle
                .submit(client::protocol::commands::ClientCommand::NewSession {
                    cwd: None,
                    model: None,
                })
                .await
                .unwrap();
            let before = handle.workspace_state().await;
            let b = format!("{}/b", directories.projects_guest_path);
            handle
                .change_workspace(b.clone(), before.session_id.clone())
                .await
                .unwrap();
            let moved = handle.workspace_state().await;
            (
                before.session_id,
                moved.transcript_path,
                before.project_root,
                b,
            )
        });
        drop(handle);
        let handle = new_engine_with_streaming(
            cfg,
            platform,
            Arc::new(FakeListener::default()),
            Arc::new(CollectingPermissionSink::default()),
            None,
        )
        .unwrap();
        handle.runtime().block_on(async {
            handle
                .resume_workspace_session(id.clone(), path.clone())
                .await
                .unwrap();
            let restored = handle.workspace_state().await;
            assert_eq!(restored.session_id, id);
            assert_eq!(restored.transcript_path, path);
            assert_eq!(restored.cwd, b);
            assert_eq!(restored.project_root, original);
        });
    }

    #[test]
    fn resume_rejects_missing_and_escaping_recorded_workspaces_before_mutating_owner() {
        let (_temp, directories, cfg, platform) = fixture();
        let handle = new_engine_with_streaming(
            cfg,
            platform,
            Arc::new(FakeListener::default()),
            Arc::new(CollectingPermissionSink::default()),
            None,
        )
        .unwrap();
        let before = handle.workspace_state_snapshot();
        handle.runtime().block_on(async {
            for cwd in [format!("{}/missing", directories.projects_guest_path), "/root".into()] {
                let id = uuid::Uuid::new_v4().to_string();
                let path = session::jsonl::session_path(&handle.lingxi_home, &cwd, &id);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                let message = serde_json::json!({"type":"user","sessionId":id,"uuid":uuid::Uuid::new_v4().to_string(),"parentUuid":null,"cwd":cwd,"isSidechain":false,"timestamp":"2026-10-22T12:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"must retain recorded cwd"}]}});
                std::fs::write(&path, format!("{message}\n")).unwrap();
                assert!(handle.resume_workspace_session(id, path.display().to_string()).await.is_err());
                assert_eq!(handle.workspace_state_snapshot(), before);
            }
        });
    }

    #[test]
    fn workspace_snapshot_waits_for_the_identity_transition_to_finish() {
        let (_temp, _directories, cfg, platform) = fixture();
        let handle = new_engine_with_streaming(
            cfg,
            platform,
            Arc::new(FakeListener::default()),
            Arc::new(CollectingPermissionSink::default()),
            None,
        )
        .unwrap();
        handle.runtime().block_on(async {
            let before = handle.workspace_state().await;
            let lease = handle.loop_transition.lock().await;
            assert!(tokio::time::timeout(
                std::time::Duration::from_millis(10),
                handle.workspace_state()
            )
            .await
            .is_err());
            assert!(tokio::time::timeout(
                std::time::Duration::from_millis(10),
                handle.workspace_sessions(MobileSessionDiscoveryScope::Current, 100)
            )
            .await
            .is_err());
            drop(lease);
            assert_eq!(handle.workspace_state().await, before);
        });
    }
}
