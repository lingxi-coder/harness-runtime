//! Direct Mod Agent APIs use the live routing/task stores and canonical Agent
//! dispatcher. Launch admission is caller-cancellable; detached settlement has
//! its own signal and retains the Host's plugin spawn slot.

use super::{
    dispatch_tool_uses_tracked_deferred_core_with_tool, mod_tool_call_agent_result,
    with_virtual_mod_result_stage, ConversationOrchestrator, AGENT_TOOL_NAME,
};
use hooks::mods::{ModAgentSpawnContext, ModAgentSpawnInput, ModError};
use lingxi_core::host::mod_agent_list::{reduce_agent_list, AgentListSnapshot};
use lingxi_core::host::task_registry::{FieldPresence, TaskListFilter, TaskRecord};
use lingxi_core::types::{ContentBlock, MessageId, ToolUseId};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use unicode_normalization::UnicodeNormalization;

/// Native's non-Unicode JS regexp replaces each UTF-16 code unit, so an emoji
/// becomes two hyphens. This must match the actual coordinator file writer.
fn team_slug(name: &str) -> String {
    name.encode_utf16()
        .map(|unit| match unit {
            48..=57 | 97..=122 => char::from_u32(u32::from(unit)).unwrap(),
            65..=90 => char::from_u32(u32::from(unit + 32)).unwrap(),
            _ => '-',
        })
        .collect()
}

fn team_config_path(config_home: &Path, team_name: &str) -> PathBuf {
    let normalized: String = config_home.to_string_lossy().nfc().collect();
    PathBuf::from(normalized)
        .join("teams")
        .join(team_slug(team_name))
        .join("config.json")
}

/// Py's disk fallback treats missing/invalid files as null. Its member guard
/// requires both string agentId and name, before MAt tests isActive === false.
async fn inactive_members(config_home: &Path, team_name: &str) -> HashSet<String> {
    let Some(config) = tokio::fs::read(team_config_path(config_home, team_name))
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return HashSet::new();
    };
    config
        .get("members")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|member| {
            member.get("name").is_some_and(Value::is_string)
                && member.get("isActive") == Some(&Value::Bool(false))
        })
        .filter_map(|member| member.get("agentId").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn membership_teams(rows: &[TaskRecord]) -> Vec<String> {
    let mut seen = HashSet::new();
    rows.iter()
        .filter(|row| row.task_type == "in_process_teammate" && row.status == "running")
        .filter_map(|row| row.agent_facts.as_ref())
        .filter(|facts| matches!(facts.resumable_agent_id, FieldPresence::Missing))
        .filter_map(|facts| match &facts.team_name {
            FieldPresence::Value(name) if seen.insert(name.clone()) => Some(name.clone()),
            _ => None,
        })
        .collect()
}

impl ConversationOrchestrator {
    pub(super) async fn mod_agent_list(&self) -> Result<Value, ModError> {
        let registry = self.task_registry.as_ref().ok_or_else(|| {
            ModError::Unavailable("agent.list needs a session agent registry".into())
        })?;
        let names = self.mod_agent_name_registry.as_ref().ok_or_else(|| {
            ModError::Unavailable("agent.list needs the live Agent name registry".into())
        })?;
        let name_entries = names.list().await;
        let task_rows = registry
            .list(TaskListFilter::default())
            .await
            .map_err(|error| {
                ModError::Unavailable(format!("agent.list cannot read the task registry: {error}"))
            })?;
        let teams = membership_teams(&task_rows);
        let config_home = self.config_home.clone().or_else(|| {
            dirs::home_dir().map(|home| {
                branding::config_home(&home, std::env::var_os(branding::CONFIG_DIR_ENV))
            })
        });
        let inactive = if teams.is_empty() {
            Some(HashSet::new())
        } else if let Some(config_home) = config_home {
            Some(
                futures::future::join_all(
                    teams
                        .iter()
                        .map(|team| inactive_members(&config_home, team)),
                )
                .await
                .into_iter()
                .flatten()
                .collect(),
            )
        } else {
            None
        };
        let reduced = reduce_agent_list(AgentListSnapshot {
            task_rows: &task_rows,
            name_entries: &name_entries,
            inactive_teammate_addresses: inactive.as_ref(),
        });
        for fact in &reduced.unresolved_facts {
            tracing::debug!(
                task_id = fact.task_id,
                field = fact.field,
                "Mod Agent list source fact is unavailable"
            );
        }
        serde_json::to_value(reduced.entries).map_err(|error| {
            ModError::Protocol(format!("agent.list serialization failed: {error}"))
        })
    }

    pub(super) async fn mod_agent_spawn(
        &self,
        input: ModAgentSpawnInput,
        context: ModAgentSpawnContext,
    ) -> Result<Value, ModError> {
        let plugin = match &context.provenance.hook_caller {
            FieldPresence::Value(Value::String(plugin)) => plugin.clone(),
            _ => {
                return Err(ModError::Protocol(
                    "agent.spawn needs its trusted plugin caller".into(),
                ))
            }
        };
        let registry = self.task_registry.as_ref().ok_or_else(|| {
            ModError::Unavailable("agent.spawn needs a session agent registry".into())
        })?;
        let Some(tool) = self.resolve_mod_tool_call_tool(AGENT_TOOL_NAME).await else {
            return Err(ModError::Hook(format!(
                "{plugin}: $.tool.call: no tool named \"Agent\" in this session"
            )));
        };
        let mut arguments = input.as_json();
        arguments.as_object_mut().unwrap().remove("tool");
        let tool_use_id = ToolUseId::from(format!("toolu_plugin_{}", uuid::Uuid::new_v4()));
        let tool_uses = vec![(tool_use_id.clone(), tool.name().to_owned(), arguments, None)];
        let (dispatch, stage) = with_virtual_mod_result_stage(
            &tool_use_id,
            dispatch_tool_uses_tracked_deferred_core_with_tool(
                self,
                &tool_uses,
                Some(context.request_cancellation.clone()),
                Some(MessageId::new()),
                1,
                None,
                Some(context.provenance.clone()),
                tool,
                None,
            ),
        )
        .await;
        let dispatch = dispatch.map_err(|error| {
            ModError::Hook(format!("{plugin}: $.tool.call(Agent) failed: {error}"))
        })?;
        let Some(ContentBlock::ToolResult { content, is_error, .. }) = dispatch.results.iter().find(|block| {
            matches!(block, ContentBlock::ToolResult { tool_use_id: id, .. } if id == &tool_use_id)
        }) else {
            return Err(ModError::Hook(format!("{plugin}: $.tool.call(Agent) produced no result")));
        };
        if stage.denial_kind().is_some() {
            let reason = content.strip_prefix("<tool_use_error>").unwrap_or(content);
            let reason = reason.strip_suffix("</tool_use_error>").unwrap_or(reason);
            return Ok(serde_json::json!({"deny":reason}));
        }
        let raw_result = stage
            .tool_use_result
            .unwrap_or_else(|| Value::String(content.clone()));
        if raw_result.get("status").and_then(Value::as_str) == Some("async_launched") {
            if let Some(agent_id) = raw_result.get("agentId").and_then(Value::as_str) {
                let result = mod_tool_call_agent_result(agent_id, &raw_result);
                let agent_id = agent_id.to_owned();
                let registry = Arc::clone(registry);
                let lease = context.retain_left_running_lease()?;
                // BOt waits without the API caller's signal or a finite timer.
                // Capturing only this registry/lease avoids retaining a whole
                // Orchestrator after session shutdown.
                tokio::spawn(async move {
                    let _lease = lease;
                    if let Err(error) = registry
                        .wait_for_agent_terminal(
                            &agent_id,
                            tokio_util::sync::CancellationToken::new(),
                            None,
                        )
                        .await
                    {
                        tracing::warn!(%plugin, %agent_id, %error, "Mod detached Agent settlement failed");
                    }
                });
                return Ok(serde_json::json!({"result":result,"text":""}));
            }
        }
        let mut result = serde_json::Map::new();
        result.insert(
            "result".into(),
            self.mod_agent_teammate_result(raw_result, registry).await?,
        );
        result.insert("text".into(), Value::String(content.clone()));
        if is_error.unwrap_or(false) {
            result.insert("isError".into(), Value::Bool(true));
        }
        Ok(Value::Object(result))
    }

    async fn mod_agent_teammate_result(
        &self,
        mut result: Value,
        registry: &Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>,
    ) -> Result<Value, ModError> {
        if result.get("status").and_then(Value::as_str) != Some("teammate_spawned") {
            return Ok(result);
        }
        let Some(teammate_id) = result.get("teammate_id").and_then(Value::as_str) else {
            return Ok(result);
        };
        let rows = registry
            .list(TaskListFilter::default())
            .await
            .map_err(|error| {
                ModError::Unavailable(format!(
                    "agent.spawn cannot read teammate identity: {error}"
                ))
            })?;
        let matches: Vec<_> = rows
            .iter()
            .filter(|row| {
                row.agent_facts.as_ref().is_some_and(|facts| {
                    facts.teammate_id == FieldPresence::Value(Value::String(teammate_id.to_owned()))
                })
            })
            .collect();
        let row = matches
            .iter()
            .find(|row| !matches!(row.status.as_str(), "completed" | "failed" | "killed"))
            .copied()
            .or_else(|| matches.last().copied());
        let id = row
            .and_then(|row| row.agent_facts.as_ref())
            .and_then(|facts| match &facts.stable_agent_id {
                FieldPresence::Value(id) => Some(id.as_str()),
                _ => None,
            })
            .unwrap_or(teammate_id)
            .to_owned();
        result["agentId"] = Value::String(id);
        if result.get("model").and_then(Value::as_str).is_some() {
            let facts = row
                .and_then(|row| row.agent_facts.as_ref())
                .ok_or_else(|| {
                    ModError::Unavailable("agent.spawn has no captured teammate route facts".into())
                })?;
            result["resolvedModel"] = Value::String(self.mod_agent_child_resolved_model(facts)?);
        }
        Ok(result)
    }

    fn mod_agent_child_resolved_model(
        &self,
        facts: &lingxi_core::host::task_registry::AgentTaskFacts,
    ) -> Result<String, ModError> {
        let FieldPresence::Value(model) = &facts.child_model else {
            return Err(ModError::Unavailable(
                "agent.spawn has no captured teammate model".into(),
            ));
        };
        let profile = match &facts.child_model_profile {
            FieldPresence::Value(profile) => Some(profile.as_str()),
            FieldPresence::Missing | FieldPresence::Null => None,
        };
        let provider = self
            .model_resolution_context_provider
            .as_ref()
            .ok_or_else(|| {
                ModError::Unavailable("agent.spawn has no model route authority".into())
            })?;
        let context = provider
            .context_for_route(model, profile)
            .map_err(|error| ModError::Unavailable(error.to_string()))?;
        agent::model_resolution::resolve_user_specified_model(model, &context)
            .map_err(|error| ModError::Unavailable(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route_test_orchestrator() -> ConversationOrchestrator {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
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
    }

    #[test]
    fn teammate_model_uses_captured_child_route_instead_of_parent_or_payload() {
        use agent::model_resolution::{
            FamilyModelDefaults, ModelProviderKind, ModelResolutionContext, ModelRouteFacts,
        };
        use lingxi_core::host::task_registry::AgentTaskFacts;
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let lookup = observed.clone();
        let orch = route_test_orchestrator().with_model_resolution_context_provider(Arc::new(
            move |model: &str, profile: Option<&str>| {
                lookup
                    .lock()
                    .unwrap()
                    .push((model.to_owned(), profile.map(str::to_owned)));
                Ok(ModelResolutionContext {
                    route: ModelRouteFacts {
                        model: model.to_owned(),
                        profile: profile.map(str::to_owned),
                        provider: Some(ModelProviderKind::Bedrock),
                        ..Default::default()
                    },
                    family_defaults: FamilyModelDefaults {
                        sonnet: Some("child-profile-sonnet".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                })
            },
        ));
        let facts = AgentTaskFacts {
            child_model: FieldPresence::Value("sonnet".into()),
            child_model_profile: FieldPresence::Value("actual-child-profile".into()),
            ..Default::default()
        };
        assert_eq!(
            orch.mod_agent_child_resolved_model(&facts).unwrap(),
            "child-profile-sonnet"
        );
        assert_eq!(
            *observed.lock().unwrap(),
            vec![
                ("claude-opus-4-8".into(), None),
                ("sonnet".into(), Some("actual-child-profile".into())),
            ]
        );
    }

    #[test]
    fn missing_or_ambiguous_child_route_is_unavailable() {
        use agent::model_resolution::ModelResolutionError;
        use lingxi_core::host::task_registry::AgentTaskFacts;
        let facts = AgentTaskFacts {
            child_model: FieldPresence::Value("shared-model".into()),
            ..Default::default()
        };
        assert!(matches!(
            route_test_orchestrator().mod_agent_child_resolved_model(&facts),
            Err(ModError::Unavailable(_))
        ));
        let orch = route_test_orchestrator().with_model_resolution_context_provider(Arc::new(
            |model: &str, profile: Option<&str>| {
                assert!(profile.is_none());
                Err(ModelResolutionError::AmbiguousRoute {
                    model: model.into(),
                    profiles: vec!["provider-a".into(), "provider-b".into()],
                })
            },
        ));
        assert!(matches!(
            orch.mod_agent_child_resolved_model(&facts),
            Err(ModError::Unavailable(_))
        ));
    }

    #[test]
    fn native_team_path_uses_utf16_slug_and_nfc_config_home() {
        assert_eq!(team_slug("Review A/中😀"), "review-a----");
        assert_eq!(
            team_config_path(Path::new("/tmp/e\u{301}"), "A😀B"),
            PathBuf::from("/tmp/é/teams/a--b/config.json")
        );
    }

    #[tokio::test]
    async fn membership_reads_actual_false_and_ignores_invalid_members_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = team_config_path(dir.path(), "Team😀");
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"members":[
                {"agentId":"idle@team","name":"idle","isActive":false,"extra":1},
                {"agentId":"live@team","name":"live","isActive":true},
                {"agentId":"null@team","name":"null","isActive":null},
                {"agentId":"number@team","name":"number","isActive":0},
                {"agentId":"nameless@team","isActive":false},
                {"agentId":3,"name":"bad-id","isActive":false},
                null
            ]}))
            .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            inactive_members(dir.path(), "Team😀").await,
            HashSet::from(["idle@team".into()])
        );
        tokio::fs::write(&path, b"{bad-json").await.unwrap();
        assert!(inactive_members(dir.path(), "Team😀").await.is_empty());
        tokio::fs::remove_file(&path).await.unwrap();
        assert!(inactive_members(dir.path(), "Team😀").await.is_empty());
    }
}
