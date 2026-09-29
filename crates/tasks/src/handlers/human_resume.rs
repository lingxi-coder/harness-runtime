//! Explicit human restoration reuses the ordinary persistent worker pump.
use super::*;

struct RestoredWorktreeGuard {
    manager: Arc<dyn lingxi_core::host::worktree::WorktreeManager>,
    handle: Option<lingxi_core::host::worktree::WorktreeHandle>,
}
impl Drop for RestoredWorktreeGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let manager = self.manager.clone();
            tokio::spawn(async move {
                let _ =
                    lingxi_core::host::worktree::agent_worktree_result(manager.as_ref(), &handle)
                        .await;
            });
        }
    }
}

impl LocalAgentHandler {
    pub(super) async fn prepare_human_restore(
        &self,
        task_id: &str,
        agent_id: AgentId,
        epoch: u64,
        ctx: TaskContext,
    ) -> Result<crate::task_trait::HumanResumePrepared, TaskError> {
        let streaming = self
            .streaming_spawner
            .as_ref()
            .ok_or(TaskError::Unsupported)?;
        let (mut request, inheritance) = self
            .resume_recipes
            .lock()
            .unwrap()
            .get(task_id)
            .cloned()
            .ok_or_else(|| {
                TaskError::Internal("original agent launch configuration is unavailable".into())
            })?;
        if let Some(gate) = &self.fork_resume_gate {
            gate.check_resume(agent_id, request.forked_skill_name.as_deref())
                .await
                .map_err(TaskError::Internal)?;
        } else if request.forked_skill_name.is_some() {
            return Err(TaskError::Internal(
                "forked agent resume permission gate unavailable".into(),
            ));
        }
        let transcript = streaming
            .transcript_path(agent_id)
            .ok_or_else(|| TaskError::Internal("agent transcript unavailable".into()))?;
        let transcript_name = transcript
            .to_str()
            .ok_or_else(|| TaskError::Internal("invalid agent transcript path".into()))?;
        let text = ctx
            .fs
            .read_file(transcript_name, None, None)
            .await
            .map_err(|error| TaskError::Io(error.to_string()))?
            .content;
        let mut history = Vec::new();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let value: serde_json::Value = serde_json::from_str(line).map_err(|error| {
                TaskError::Internal(format!("agent transcript is incomplete: {error}"))
            })?;
            if let Some(model) = value
                .get("model")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
            {
                request.model = Some(model.to_owned());
                request.model_profile = value
                    .get("model_profile")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
            }
            if let Some(message) = value.get("message") {
                history.push(
                    serde_json::from_value::<lingxi_core::types::ConversationMessage>(
                        message.clone(),
                    )
                    .map_err(|error| {
                        TaskError::Internal(format!("invalid agent transcript message: {error}"))
                    })?,
                );
            }
        }
        if history.is_empty() {
            return Err(TaskError::Internal(
                "agent transcript has no messages to resume".into(),
            ));
        }
        let mut created_worktree = None;
        if let Some(original) = request.worktree.clone() {
            let manager = self.worktree_manager.as_ref().ok_or_else(|| {
                TaskError::Internal("worktree manager unavailable for isolated resume".into())
            })?;
            let handle = if original.path.exists() {
                manager
                    .enter_existing(&original.path)
                    .await
                    .map_err(|error| TaskError::Internal(error.to_string()))?
            } else {
                let base = original.base_commit.as_deref().ok_or_else(|| {
                    TaskError::Internal("removed worktree has no pinned base commit".into())
                })?;
                let slug = original
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| TaskError::Internal("invalid worktree identity".into()))?;
                // A git creation can complete after its caller is cancelled.
                // The detached producer owns cleanup until the receiver accepts it.
                let manager = manager.clone();
                let slug = slug.to_owned();
                let base = base.to_owned();
                let (created, receiver) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    let result = manager.create_worktree(&slug, Some(&base), &[]).await;
                    let result = result.map(|handle| RestoredWorktreeGuard {
                        manager,
                        handle: Some(handle),
                    });
                    let _ = created.send(result);
                });
                let guarded = receiver
                    .await
                    .map_err(|_| TaskError::Internal("worktree creation was interrupted".into()))?
                    .map_err(|error| TaskError::Internal(error.to_string()))?;
                let handle = guarded.handle.as_ref().expect("new worktree owned").clone();
                created_worktree = Some(guarded);
                handle
            };
            if let Some(cwd) = request.cwd.as_ref() {
                let old = std::path::Path::new(cwd);
                if let Ok(suffix) = old.strip_prefix(&original.path) {
                    request.cwd = Some(handle.path.join(suffix).to_string_lossy().into_owned());
                }
            } else {
                request.cwd = Some(handle.path.to_string_lossy().into_owned());
            }
            request.worktree = Some(handle);
        }
        if let Some(cwd) = request.cwd.as_ref() {
            if !std::path::Path::new(cwd).is_dir() {
                return Err(TaskError::Internal(
                    "agent working directory no longer exists".into(),
                ));
            }
        }
        request.resumed_history = Some(history);
        request.prompt.clear();
        request.run_in_background = true;
        let input = TaskSpawnInput::LocalAgent {
            agent_id,
            subagent_type: request.subagent_type.clone(),
            prompt: String::new(),
            is_backgrounded: true,
            tool_use_id: request.tool_use_id.clone(),
            creator_teammate_name: request.creator_teammate_name.clone(),
            creator_team_name: request.creator_team_name.clone(),
            creator_agent_id: request.creator_agent_id,
            spawn_request: Some(request),
            inheritance: Some(inheritance),
        };
        let (ready, receiver) = tokio::sync::oneshot::channel();
        let handle = self
            .spawn_inner(
                input,
                ctx,
                Some(HumanRestore {
                    task_id: task_id.to_owned(),
                    epoch,
                    ready,
                }),
            )
            .await?;
        // The shared worker now owns the same terminal worktree cleanup.
        if let Some(guard) = created_worktree.as_mut() {
            guard.handle = None;
        }
        Ok(crate::task_trait::HumanResumePrepared {
            handle,
            ready: receiver,
        })
    }
}
