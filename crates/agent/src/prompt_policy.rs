//! Child prompt rendering from one resolved request policy.

use crate::api::SubagentApiClient;
use crate::handle::{
    PoolSubagentSpawner, SubagentEnvRenderer, SubagentPromptContext, SubagentPromptTarget,
};
use lingxi_core::host::prompt_policy::{
    resolve_prompt_policy, ResolvedPromptPolicy, SystemPromptBinding,
};
use lingxi_core::host::session_context::ConversationMode;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
use std::path::PathBuf;
use std::sync::Arc;

pub fn resolve_target(
    api: Option<&dyn SubagentApiClient>,
    model: &str,
    profile: Option<&str>,
    mode: ConversationMode,
) -> Result<SubagentPromptTarget, llm_runtime::LlmError> {
    let target = match api {
        Some(api) => api.prompt_target(model, profile, mode)?,
        None => lingxi_core::host::prompt_policy::PromptTarget::unresolved(model, profile, mode),
    };
    let supports_system_turns = api.map_or_else(
        || lingxi_core::host::model_capabilities::prompt_system_turns_for(model),
        |api| api.supports_system_turns(model, profile),
    );
    let policy = Arc::new(resolve_prompt_policy(target));
    tracing::debug!(
        provider = %policy.target.provider_id,
        model = %policy.target.canonical_model,
        profile = ?profile,
        mode = %mode.as_str(),
        bundle = %policy.bundle_id,
        version = %policy.bundle_version,
        rule = %policy.matched_rule,
        "resolved child prompt policy",
    );
    Ok(SubagentPromptTarget {
        model: model.to_owned(),
        model_profile: profile.map(str::to_owned),
        policy,
        supports_system_turns,
    })
}

#[cfg(test)]
mod tests;

/// Sources, rather than a previous rendered prompt, survive a model change.
#[derive(Clone)]
pub struct SubagentPromptState {
    pub initial_policy: Arc<ResolvedPromptPolicy>,
    pub role: Option<Utf16JsonProjection>,
    pub fork_prefix: Option<Utf16JsonProjection>,
    pub fork_policy: Option<lingxi_core::host::prompt_policy::PromptPolicyMetadata>,
    pub custom_system_prompt: Option<Utf16JsonProjection>,
    pub append_system_prompt: Option<Utf16JsonProjection>,
    pub environment: Option<SubagentEnvRenderer>,
    pub cwd: Option<PathBuf>,
    pub addendum: Option<Utf16JsonProjection>,
    pub operator_suffix: Option<String>,
    pub tool_registry: Option<Arc<tool_api::ToolRegistry>>,
    pub system_renderer: Option<Arc<dyn SubagentSystemPromptRenderer>>,
    pub seeded_context: PromptContextSeed,
    pub replace_initial_context: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PromptContextSeed {
    pub user_message: Option<MessageId>,
    pub user_prefix: Vec<ContentBlock>,
    pub system_messages: Vec<MessageId>,
}

#[async_trait::async_trait]
pub trait SubagentSystemPromptRenderer: Send + Sync {
    async fn render_system_prompt(
        &self,
        target: &SubagentPromptTarget,
    ) -> Result<Utf16JsonProjection, lingxi_core::settings::SettingsError>;

    async fn render_prompt_context(
        &self,
        _target: &SubagentPromptTarget,
    ) -> Result<SubagentPromptContext, lingxi_core::settings::SettingsError> {
        Ok(SubagentPromptContext::default())
    }
}

pub struct RenderedSubagentPrompt {
    pub policy: Arc<ResolvedPromptPolicy>,
    pub system: Option<Arc<Utf16JsonProjection>>,
    pub context: Vec<String>,
    pub system_turns: bool,
    pub reuses_fork_prefix: bool,
}

#[derive(Clone)]
pub(crate) struct CapturedRequestPrompt {
    pub policy: Arc<ResolvedPromptPolicy>,
    pub system: Option<Utf16JsonProjection>,
}

pub(crate) type RequestPromptCaptures =
    Arc<std::sync::Mutex<std::collections::HashMap<String, CapturedRequestPrompt>>>;

fn append_text(
    body: &mut Option<Utf16JsonProjection>,
    text: &Utf16JsonProjection,
    separator: &str,
) {
    if let Some(current) = body {
        let mut units = current.string_units("").expect("system body is a string");
        units.extend(separator.encode_utf16());
        units.extend(text.string_units("").expect("system suffix is a string"));
        *current = Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
            .expect("system text retains its UTF-16 source");
    } else {
        *body = Some(text.clone());
    }
}

fn append_portable_reminders(
    body: &mut Option<Utf16JsonProjection>,
    policy: &ResolvedPromptPolicy,
) {
    for key in ["subagent_consent", "subagent_notes"] {
        if let Some(text) = policy.reminders.template_for(key) {
            append_text(body, &Utf16JsonProjection::from(text.trim_end()), "\n\n");
        }
    }
}

impl SubagentPromptState {
    pub async fn render_for_route(
        &self,
        target: &SubagentPromptTarget,
    ) -> Result<RenderedSubagentPrompt, lingxi_core::settings::SettingsError> {
        let mut rendered = self.render(target);
        if !rendered.reuses_fork_prefix && self.custom_system_prompt.is_none() {
            if let Some(renderer) = &self.system_renderer {
                let mut body = Some(renderer.render_system_prompt(target).await?);
                // Host-rendered teammates retain their complete target-specific
                // sections, plus the same portable child constraints as a
                // standalone generic child. Native teammate bytes stay intact.
                if !target.policy.reminders.is_native() {
                    append_portable_reminders(&mut body, &target.policy);
                }
                let context = renderer.render_prompt_context(target).await?;
                rendered.context.extend(context.additional_context);
                if let Some(suffix) = context.system_suffix {
                    append_text(&mut body, &Utf16JsonProjection::from(suffix), "\n\n");
                }
                rendered.system = body.map(Arc::new);
            }
        }
        Ok(rendered)
    }

    #[must_use]
    pub fn render(&self, target: &SubagentPromptTarget) -> RenderedSubagentPrompt {
        let reuses_fork_prefix = self.fork_prefix.is_some()
            && self
                .fork_policy
                .as_ref()
                .is_some_and(|policy| *policy == target.policy.metadata());
        let native = target.policy.reminders.is_native();
        let uses_custom_override = !reuses_fork_prefix && self.custom_system_prompt.is_some();
        let mut body = if reuses_fork_prefix {
            self.fork_prefix.clone()
        } else if uses_custom_override {
            self.custom_system_prompt.clone()
        } else {
            match target.policy.system {
                SystemPromptBinding::Template(template) => {
                    let mut body = Some(Utf16JsonProjection::from(template.trim_end()));
                    if let Some(role) = &self.role {
                        append_text(&mut body, role, "\n\n");
                    }
                    body
                }
                SystemPromptBinding::ClaudeCode | SystemPromptBinding::ClaudeChat => {
                    self.role.clone()
                }
            }
        };
        let context = if reuses_fork_prefix {
            SubagentPromptContext::default()
        } else {
            if body.is_some() && !uses_custom_override {
                if native {
                    let rewrite = lingxi_core::host::model_capabilities::lean_rewrite_for(
                        &target.policy.target.canonical_model,
                    );
                    let (consent, notes) = PoolSubagentSpawner::subagent_prompt_appendix(rewrite);
                    append_text(&mut body, &Utf16JsonProjection::from(consent), "\n\n");
                    append_text(&mut body, &Utf16JsonProjection::from(notes), "\n\n");
                } else {
                    append_portable_reminders(&mut body, &target.policy);
                }
            }
            self.environment
                .as_ref()
                .map_or_else(SubagentPromptContext::default, |render| {
                    render(target, self.cwd.as_deref())
                })
        };
        if body.is_some() {
            if let Some(suffix) = context
                .system_suffix
                .as_ref()
                .filter(|_| !uses_custom_override)
            {
                append_text(
                    &mut body,
                    &Utf16JsonProjection::from(suffix.as_str()),
                    "\n\n",
                );
            }
            if !reuses_fork_prefix {
                if let Some(append) = &self.append_system_prompt {
                    append_text(&mut body, append, "\n\n");
                }
            }
            if let Some(addendum) = &self.addendum {
                append_text(&mut body, addendum, "");
            }
            if let Some(suffix) = &self.operator_suffix {
                append_text(
                    &mut body,
                    &Utf16JsonProjection::from(suffix.as_str()),
                    "\n\n",
                );
            }
        }
        RenderedSubagentPrompt {
            policy: target.policy.clone(),
            system: body.map(Arc::new),
            context: context.additional_context,
            system_turns: matches!(
                target.policy.system,
                SystemPromptBinding::ClaudeCode | SystemPromptBinding::ClaudeChat
            ) && lingxi_core::host::model_capabilities::prompt_system_turns_for_route(
                &target.policy.target.canonical_model,
                target.supports_system_turns,
            ),
            reuses_fork_prefix,
        }
    }

    /// Replace only engine-owned environment context in this request projection.
    /// Return the source state for this exact projection. A downstream model
    /// rewrite must use its complete seed, including optional context sections.
    pub fn project_context(
        &self,
        messages: &mut Vec<ConversationMessage>,
        rendered: &RenderedSubagentPrompt,
    ) -> Self {
        if rendered.reuses_fork_prefix
            || (!self.replace_initial_context
                && self.initial_policy.as_ref() == rendered.policy.as_ref())
        {
            return self.clone();
        }
        self.seeded_context.remove(messages);
        // Forked and recovered history may have been written by an older
        // process; the Native three-section envelope is unambiguous here.
        remove_native_environment_context(messages);
        let mut projected = self.clone();
        projected.seeded_context =
            inject_context(messages, &rendered.context, rendered.system_turns);
        projected.initial_policy = rendered.policy.clone();
        projected.replace_initial_context = false;
        projected
    }
}

impl PromptContextSeed {
    fn remove(&self, messages: &mut Vec<ConversationMessage>) {
        messages.retain(|message| !self.system_messages.contains(&message.id()));
        if let Some(id) = self.user_message {
            if let Some(ConversationMessage::User { content, .. }) =
                messages.iter_mut().find(|row| row.id() == id)
            {
                // A previous fallback may already have removed this prefix.
                // Never consume the assignment now occupying the same user row.
                if content.starts_with(&self.user_prefix) {
                    content.drain(..self.user_prefix.len());
                }
            }
        }
    }
}

pub fn inject_context(
    messages: &mut Vec<ConversationMessage>,
    sections: &[String],
    system_turns: bool,
) -> PromptContextSeed {
    if sections.is_empty() {
        return PromptContextSeed::default();
    }
    if system_turns {
        let id = MessageId::new();
        messages.push(ConversationMessage::api_system(
            id,
            lingxi_core::types::ApiSystemMessage {
                content: vec![ContentBlock::Text {
                    text: sections.join("\n\n"),
                    citations: None,
                }],
                output_config: None,
            },
        ));
        PromptContextSeed {
            system_messages: vec![id],
            ..Default::default()
        }
    } else if let Some(ConversationMessage::User { id, content, .. }) = messages
        .iter_mut()
        .find(|row| matches!(row, ConversationMessage::User { .. }))
    {
        let mut prefix = sections
            .iter()
            .enumerate()
            .map(|(index, text)| ContentBlock::Text {
                text: format!(
                    "<system-reminder>\n{text}\n</system-reminder>{}",
                    if index + 1 == sections.len() {
                        "\n"
                    } else {
                        ""
                    }
                ),
                citations: None,
            })
            .collect::<Vec<_>>();
        let seeded_prefix = prefix.clone();
        prefix.append(content);
        *content = prefix;
        PromptContextSeed {
            user_message: Some(*id),
            user_prefix: seeded_prefix,
            system_messages: Vec::new(),
        }
    } else {
        let id = MessageId::new();
        messages.push(ConversationMessage::user_meta(id, sections.join("\n\n")));
        PromptContextSeed {
            system_messages: vec![id],
            ..Default::default()
        }
    }
}

pub(crate) fn remove_native_environment_context(messages: &mut Vec<ConversationMessage>) {
    messages.retain(|message| {
        let ConversationMessage::System {
            api_system: Some(api),
            ..
        } = message
        else {
            return true;
        };
        let text = api
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        !(text.starts_with("# Environment\n")
            && text.contains("\n\nYou are powered by the model")
            && text.contains("\n\nToday's date is "))
    });
    for message in messages {
        let ConversationMessage::User { content, .. } = message else {
            continue;
        };
        let prefix = [
            "<system-reminder>\n# Environment\n",
            "<system-reminder>\nYou are powered by the model",
            "<system-reminder>\nToday's date is ",
        ];
        let matches = content.len() >= prefix.len()
            && content
                .iter()
                .zip(prefix)
                .all(|(block, expected)| match block {
                    ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } => {
                        text.starts_with(expected)
                    }
                    _ => false,
                });
        if matches {
            content.drain(..prefix.len());
        } else if content.len() >= 3
            && content[0]
                .visible_text()
                .is_some_and(|text| text.starts_with("<system-reminder>\n# Environment\n"))
            && content[1]
                .visible_text()
                .is_some_and(|text| text.starts_with("<system-reminder>\nModel ID: "))
            && content[2]
                .visible_text()
                .is_some_and(|text| text.starts_with("<system-reminder>\nToday's date is "))
        {
            content.drain(..3);
        }
    }
}

/// Re-render the already admitted tool set; a route change does not grant tools.
pub async fn rerender_tools(
    context: &crate::context::SubagentContext,
    target: &SubagentPromptTarget,
    previous: &[lingxi_core::host::SourceToolWireDefinition],
) -> Result<Vec<lingxi_core::host::SourceToolWireDefinition>, lingxi_core::settings::SettingsError>
{
    let Some(registry) = context
        .prompt_state
        .as_ref()
        .and_then(|state| state.tool_registry.as_ref())
    else {
        return Ok(previous.to_vec());
    };
    let mut tools = Vec::new();
    for schema in previous {
        // A schema-bearing workflow owns this tool's narrowed input schema.
        if schema.name == "StructuredOutput" && context.schema.is_some() {
            continue;
        }
        let supplied = context
            .supplied_mcp_tools
            .iter()
            .find(|tool| tool.name() == schema.name);
        let private = context
            .handback
            .as_ref()
            .filter(|_| schema.name == lingxi_core::host::handback::HANDBACK_TOOL_NAME)
            .map(|runtime| {
                Arc::new(crate::handback::SubagentHandbackTool(runtime.clone()))
                    as Arc<dyn tool_api::Tool>
            });
        if let Some(tool) = supplied
            .cloned()
            .or(private)
            .or_else(|| registry.find_by_name(&schema.name))
        {
            tools.push(tool);
        }
    }
    let bash_precommit_skills = if tools
        .iter()
        .any(|tool| tool.name() == "Skill" || tool.aliases().contains(&"Skill"))
    {
        registry.bash_precommit_skills().await?
    } else {
        Default::default()
    };
    let fresh = tool_api::wire::tools_to_wire(
        &tools,
        &tool_api::tool_trait::PromptOptions {
            prompt_policy: Some(target.policy.clone()),
            include_examples: true,
            model: Some(target.model.clone()),
            model_profile: target.model_profile.clone(),
            bash_precommit_skills,
            bash_precommit_session_generation: registry.bash_precommit_session_generation(),
            ..Default::default()
        },
    )
    .await;
    Ok(previous
        .iter()
        .map(|schema| {
            fresh
                .iter()
                .find(|new| new.name == schema.name)
                .cloned()
                .unwrap_or_else(|| schema.clone())
        })
        .collect())
}
