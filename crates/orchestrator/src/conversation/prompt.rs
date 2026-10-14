//! System-prompt and leading-context assembly.

use super::*;
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use memory::lingxi_md::LingxiMdTier;

fn mod_instruction_files(
    files: &[crate::prompt::MemoryFile],
    native_managed: &[lingxi_core::host::instructions::InstructionFile],
) -> Vec<Utf16JsonProjection> {
    use lingxi_core::host::instructions::InstructionFileType;

    let insertion_index = crate::prompt::memory_block::native_managed_insertion_index(files);
    let mut entries = Vec::new();
    let mut inserted_native = false;
    for (index, file) in files.iter().enumerate() {
        if !inserted_native && index >= insertion_index {
            entries.extend(native_managed.iter().filter_map(|row| {
                (row.kind == InstructionFileType::Managed).then(|| {
                    projection_object(vec![
                        ("path".into(), row.path.clone()),
                        ("kind".into(), "managed".into()),
                        ("content".into(), row.content.clone()),
                    ])
                })
            }));
            inserted_native = true;
        }
        if !crate::prompt::memory_block::is_rendered_into_context(file) {
            continue;
        }
        let kind = match file.tier {
            LingxiMdTier::Managed => "managed",
            LingxiMdTier::User => "user",
            LingxiMdTier::Project => "project",
            LingxiMdTier::Local => "local",
        };
        let mut fields = vec![
            (
                "path".into(),
                crate::prompt::memory_block::path_projection(&file.path),
            ),
            ("kind".into(), kind.into()),
            (
                "content".into(),
                file.source_content.as_deref().unwrap_or(&file.body).into(),
            ),
        ];
        if let Some(parent) = &file.parent {
            fields.push((
                "parent".into(),
                crate::prompt::memory_block::path_projection(parent),
            ));
        }
        entries.push(projection_object(fields));
    }
    if !inserted_native {
        entries.extend(native_managed.iter().filter_map(|row| {
            (row.kind == InstructionFileType::Managed).then(|| {
                projection_object(vec![
                    ("path".into(), row.path.clone()),
                    ("kind".into(), "managed".into()),
                    ("content".into(), row.content.clone()),
                ])
            })
        }));
    }
    entries
}

fn generated_projection(value: &serde_json::Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(value).expect("host-generated Mod input is valid JSON")
}

fn projection_object(fields: Vec<(String, Utf16JsonProjection)>) -> Utf16JsonProjection {
    Utf16JsonProjection::object(fields).expect("host-generated Mod object is valid")
}

fn projection_array(values: Vec<Utf16JsonProjection>) -> Utf16JsonProjection {
    Utf16JsonProjection::array(values).expect("host-generated Mod array is valid")
}

fn typed_attachment_ids_in_snapshot(
    typed_ids: &std::collections::HashSet<lingxi_core::types::MessageId>,
    messages: &[ConversationMessage],
) -> std::collections::HashSet<lingxi_core::types::MessageId> {
    let snapshot_ids = messages
        .iter()
        .map(ConversationMessage::id)
        .collect::<std::collections::HashSet<_>>();
    typed_ids.intersection(&snapshot_ids).copied().collect()
}

fn exact_string_projection(display: &str, code_units: &[u16]) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(display.to_owned(), code_units.to_vec())
        .expect("display text matches exact UTF-16 source units")
}

fn projection_string(projection: &Utf16JsonProjection, pointer: &str) -> Option<String> {
    String::from_utf16(&projection.string_units(pointer)?).ok()
}

fn projection_prompt_text(
    projection: &Utf16JsonProjection,
    pointer: &str,
) -> Option<lingxi_llm_client::providers::anthropic::system_prompt::PromptText> {
    projection
        .string_units(pointer)
        .map(lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_utf16)
}

fn prompt_text_projection(
    text: &lingxi_llm_client::providers::anthropic::system_prompt::PromptText,
) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(
        text.display_text().to_owned(),
        text.utf16_code_units().to_vec(),
    )
    .expect("PromptText carries a valid exact UTF-16 projection")
}

fn append_source_system_prompt(
    prompt: &mut crate::prompt::RenderedPromptSections,
    source: &Utf16JsonProjection,
) {
    let Some(units) = source.string_units("").filter(|units| !units.is_empty()) else {
        return;
    };
    let display = String::from_utf16_lossy(&units);
    if !prompt.text.is_empty() {
        prompt
            .text
            .push_str(crate::prompt::locked_templates::SECTION_SEP);
    }
    prompt.text.push_str(&display);
    if !prompt.static_text.is_empty() {
        prompt
            .static_text
            .push_str(crate::prompt::locked_templates::SECTION_SEP);
    }
    prompt.static_text.push_str(&display);
    if let Some(snapshot) = prompt.snapshot_system_prompt.as_mut() {
        snapshot.push(
            lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_utf16(units),
        );
    } else {
        prompt.source_sections.push(crate::prompt::PromptSection {
            id: "append_system_prompt".to_owned(),
            text: display,
            scope: crate::prompt::PromptSectionScope::Session,
            id_utf16_code_units: None,
            text_utf16_code_units: Some(units),
        });
    }
}

fn public_prompt_name(
    name: &lingxi_llm_client::providers::anthropic::system_prompt::PromptText,
) -> lingxi_llm_client::providers::anthropic::system_prompt::PromptText {
    lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_utf16(
        name.utf16_code_units()
            .iter()
            .copied()
            .take_while(|unit| *unit != b':' as u16)
            .collect(),
    )
}

fn prompt_parts_source_text(prompt: &crate::prompt::RenderedPromptSections) -> Utf16JsonProjection {
    let mut units = Vec::new();
    if prompt.source_sections.is_empty() {
        units.extend(prompt.text.encode_utf16());
        return Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
            .expect("rendered system prompt has a valid exact UTF-16 projection");
    }
    for section in &prompt.source_sections {
        if !units.is_empty() {
            if units.last() == Some(&(b'\n' as u16)) {
                units.push(b'\n' as u16);
            } else {
                units.extend(crate::prompt::locked_templates::SECTION_SEP.encode_utf16());
            }
        }
        units.extend_from_slice(section.text_prompt().utf16_code_units());
    }
    if let Some(dynamic) = prompt.dynamic_system_context.as_ref() {
        units.extend("\n\n".encode_utf16());
        units.extend(dynamic.encode_utf16());
    }
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("rendered system prompt has a valid exact UTF-16 projection")
}

fn render_mod_prompt_context(resolved: &Utf16JsonProjection) -> Option<ConversationMessage> {
    let block_count = resolved.array_len("/blocks")?;
    let blocks = (0..block_count)
        .filter_map(|index| {
            let name = resolved.string_units(&format!("/blocks/{index}/name"))?;
            let text = resolved.string_units(&format!("/blocks/{index}/text"))?;
            let mut units = "# ".encode_utf16().collect::<Vec<_>>();
            units.extend(name);
            units.push(b'\n' as u16);
            units.extend(text);
            Some(units)
        })
        .collect::<Vec<_>>();
    if blocks.is_empty() {
        return None;
    }
    let mut body_units = Vec::new();
    for (index, block) in blocks.into_iter().enumerate() {
        if index > 0 {
            body_units.push(b'\n' as u16);
        }
        body_units.extend(block);
    }
    if body_units.is_empty() {
        return None;
    }
    // The six leading spaces are part of the model-visible bytes. Keep this
    // literal on one line because Rust line continuations strip indentation.
    let important = format!(
        "      {} attached this context automatically; it isn't part of the user's message. It describes the user's own account and workspace, so they don't need it reported back.",
        branding::PRODUCT_NAME
    );
    let prefix = "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n";
    let suffix = format!("\n\n{important}\n</system-reminder>\n");
    let mut exact_units = prefix.encode_utf16().collect::<Vec<_>>();
    exact_units.extend(body_units);
    exact_units.extend(suffix.encode_utf16());
    let content = String::from_utf16_lossy(&exact_units);
    Some(ConversationMessage::user_meta_js_utf16(
        MessageId::new(),
        content,
        exact_units,
    ))
}

/// Agent state prepared before a resumed session crosses its activation boundary.
#[derive(Debug)]
pub(crate) struct PreparedMainThreadAgentRestore {
    wanted: Option<String>,
    definition: Option<agent::AgentDefinition>,
    model_override: Option<agent::model_resolution::ResolvedModelSelection>,
    hooks_trusted: bool,
}

fn prompt_tool_descriptions(
    wire_tools: &[lingxi_core::host::SourceToolWireDefinition],
) -> Vec<lingxi_core::host::PromptToolDescription> {
    let mut seen = std::collections::HashSet::new();
    wire_tools
        .iter()
        .filter(|tool| !tool.defer_loading)
        .filter_map(|tool| {
            let name = tool.name.trim();
            if name.is_empty() || !seen.insert(name.to_owned()) {
                return None;
            }
            Some(lingxi_core::host::PromptToolDescription {
                name: name.to_owned(),
                description: tool.description.clone(),
            })
        })
        .collect()
}

fn valid_prompt_snapshot(snapshot: &lingxi_core::host::PromptSnapshot) -> bool {
    !snapshot.system_prompt.is_empty() && snapshot.tools.iter().all(|tool| !tool.name.is_empty())
}

fn mod_prompt_compose_facts(
    ctx: &crate::prompt::SystemPromptContext,
    style: Option<crate::prompt::ActiveOutputStyle<'_>>,
    surfaces: &[&str],
) -> Utf16JsonProjection {
    let lean = lingxi_core::host::model_capabilities::prompt_profile_for(&ctx.model)
        == lingxi_core::host::model_capabilities::PromptProfile::ClaudeLean;
    let mut traits = Vec::new();
    if lean {
        traits.push("lean");
    }
    if ctx.exclude_dynamic_sections {
        traits.push("sdk-preset");
    }
    if !ctx.is_interactive {
        traits.push("print");
    }
    if ctx.skills_available {
        traits.push("skills");
    }
    generated_projection(&serde_json::json!({
        "model":ctx.model,
        "promptModel":ctx.model,
        "surfaces":surfaces,
        "tools":ctx.tool_names,
        "outputStyle":style.map(|style| serde_json::json!({
            "name":style.name,
            "isKeepingCodingInstructions":style.keep_coding_instructions,
        })),
        "traits":traits,
    }))
}

fn mod_prompt_compose_result(sections: &[crate::prompt::PromptSection]) -> Utf16JsonProjection {
    let values = sections
        .iter()
        .map(|section| {
            let id = section.public_id_prompt();
            let scope = if section.scope == crate::prompt::PromptSectionScope::Shared {
                "shared"
            } else {
                "session"
            };
            Utf16JsonProjection::object(vec![
                ("id".into(), prompt_text_projection(&id)),
                (
                    "text".into(),
                    prompt_text_projection(&section.text_prompt()),
                ),
                ("scope".into(), scope.into()),
            ])
            .expect("prompt section is a valid JSON object")
        })
        .collect::<Vec<_>>();
    let sections = Utf16JsonProjection::array(values).expect("prompt sections form a JSON array");
    Utf16JsonProjection::object(vec![("sections".into(), sections)])
        .expect("prompt compose result is a valid JSON object")
}

fn validate_mod_prompt_compose_facts(
    facts: &Utf16JsonProjection,
) -> Result<(), hooks::mods::ModError> {
    let invalid = || hooks::mods::ModError::Hook("prompt.compose needs valid prompt facts".into());
    let nonempty_string = |pointer: &str| {
        facts
            .string_units(pointer)
            .is_some_and(|units| !units.is_empty())
    };
    let all_nonempty_strings = |pointer: &str| {
        facts.array_len(pointer).is_some_and(|len| {
            (0..len).all(|index| {
                facts
                    .string_units(&format!("{pointer}/{index}"))
                    .is_some_and(|units| !units.is_empty())
            })
        })
    };
    let array_all = |pointer: &str, allowed: &[&str]| {
        facts.array_len(pointer).is_some_and(|len| {
            (0..len).all(|index| {
                let units = facts.string_units(&format!("{pointer}/{index}"));
                units.is_some_and(|units| {
                    allowed
                        .iter()
                        .any(|item| units.iter().copied().eq(item.encode_utf16()))
                })
            })
        })
    };
    if facts
        .at_pointer("")
        .and_then(|root| root.object_entries())
        .is_none()
        || !nonempty_string("/model")
        || !nonempty_string("/promptModel")
        || !all_nonempty_strings("/tools")
        || !array_all("/surfaces", &["terminal", "desktop", "mobile", "vscode"])
        || !array_all(
            "/traits",
            &[
                "bare",
                "lean",
                "sdk-preset",
                "teammate",
                "analysis",
                "print",
                "skills",
                "send-user-message",
            ],
        )
    {
        return Err(invalid());
    }
    if let Some(style) = facts.get_property("outputStyle") {
        if !style.is_null()
            && (style
                .get_property("name")
                .and_then(JsonValueView::as_str)
                .is_none()
                || style
                    .get_property("isKeepingCodingInstructions")
                    .and_then(JsonValueView::as_bool)
                    .is_none())
        {
            return Err(invalid());
        }
    } else {
        return Err(invalid());
    }
    Ok(())
}

impl ConversationOrchestrator {
    pub(crate) fn mod_prompt_surfaces(&self, interactive: bool) -> Vec<&'static str> {
        let mut surfaces = if self.mobile_runtime_environment.is_some() {
            if interactive {
                vec!["mobile"]
            } else {
                Vec::new()
            }
        } else if let Some(surface) = self.config.mod_render_surface {
            vec![surface.as_str()]
        } else if interactive {
            vec!["terminal"]
        } else {
            Vec::new()
        };
        for surface in self.mod_surface_roster.surfaces() {
            let surface = surface.as_str();
            if !surfaces.contains(&surface) {
                surfaces.push(surface);
            }
        }
        surfaces
    }
    /// Assemble the system prompt this orchestrator would send on the next
    /// turn, WITHOUT running a turn (no API call, no message mutation).
    ///
    /// Honors the same `system_prompt_override` bypass as [`Self::run_turn`]:
    /// returns the override verbatim when set, otherwise the freshly assembled
    /// prompt (cwd / git / file-tree / **memory** / tool-name context). This is
    /// a read-only introspection seam — it lets a host/composition-root test
    /// prove that its injected [`crate::prompt::MemoryHierarchyProvider`]
    /// (e.g. a controlled `StaticMemoryProvider`, or the production
    /// `real_provider()`) actually reaches the system prompt, without a live
    /// model round-trip.
    pub async fn assemble_system_prompt_preview(
        &self,
    ) -> Result<String, lingxi_core::settings::SettingsError> {
        self.effective_system_prompt().await
    }

    /// Assemble the live default system prompt without applying the main
    /// thread's `--system-prompt` / adopted-agent overrides.
    ///
    /// In-process teammates use the same default prompt builder as Claude Code,
    /// then append their teammate addendum and optional custom agent prompt.
    /// Keeping this as a read-only renderer avoids copying the dynamic cwd/git/
    /// memory/tool assembly into the task-handler leaf.
    pub async fn assemble_default_system_prompt_preview(
        &self,
    ) -> Result<
        lingxi_core::types::utf16_json::Utf16JsonProjection,
        lingxi_core::settings::SettingsError,
    > {
        Ok(prompt_parts_source_text(
            &self.build_system_prompt_parts().await?,
        ))
    }

    /// Adopt a `--agent`-resolved definition for the MAIN conversation loop
    /// (claude-code `bde(agentDef.agentType)` + `mainThreadAgentDefinition`).
    /// Called ONCE at startup by the composition root when `--agent` resolves to
    /// a catalog hit — the resolution runs against the FINAL agent catalog after
    /// this orchestrator is already `Arc`-wrapped, so the seam is interior-mutable.
    /// After this, `agent_type` rides every main-thread lifecycle hook payload,
    /// `system_prompt` (when `Some`) becomes the main-loop system prompt on every
    /// query — `--system-prompt` (`system_prompt_override`) still winning — and
    /// `tool_policy` / `disallowed_tools` narrow the advertised tool pool (claude
    /// `HJ(agentDef,to,!1,!0)`).
    ///
    /// `model_override` pairs the agent's frontmatter `model`, already resolved
    /// to a concrete wire id, with its selected provider profile. The caller
    /// gates the override (claude-code
    /// `if(!userSpecifiedModel&&y.model&&y.model!=="inherit"){jb(Zo(y.model))}` —
    /// the `!userSpecifiedModel` / `!=="inherit"` checks live at the composition
    /// root, which owns `--model`). When `Some`, both route fields are applied
    /// together. An empty profile remains distinct from an absent profile.
    pub async fn set_main_thread_agent(
        &self,
        agent_type: String,
        system_prompt: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        tool_policy: agent::AgentToolPolicy,
        disallowed_tools: Vec<String>,
        model_override: Option<(String, Option<String>)>,
    ) {
        if let Some((model, profile)) = model_override {
            {
                let mut s = self.session.lock().await;
                s.model.clone_from(&model);
                s.model_profile.clone_from(&profile);
            }
            self.refresh_main_loop_model_for_route(&model, profile.as_deref());
        }
        *self.lifecycle_runtime.main_thread_agent.write().await = Some(MainThreadAgentState {
            agent_type,
            system_prompt,
            tool_policy,
            disallowed_tools,
        });
    }

    /// Replace the frontmatter-hook bucket owned by the main-thread agent.
    /// Normal startup installs it once; hot resume calls this again so the
    /// previous session's hooks are removed before the resumed agent's hooks
    /// become visible.
    pub async fn replace_main_thread_agent_hooks(&self, definitions: &[hooks::HookDefinition]) {
        let previous = self
            .lifecycle_runtime
            .main_thread_agent_hook_id
            .lock()
            .await
            .take();
        if let Some(previous) = previous {
            self.hooks.clear_agent_hooks(previous).await;
        }
        if definitions.is_empty() {
            return;
        }
        let agent_id = lingxi_core::types::AgentId::new();
        self.hooks
            .register_agent_hooks(agent_id, definitions, false)
            .await;
        *self
            .lifecycle_runtime
            .main_thread_agent_hook_id
            .lock()
            .await = Some(agent_id);
    }

    /// Resolve the agent against the target session's route before preparing
    /// or activating any session-owned state. Keep its complete definition and
    /// route so the eventual application has no second fallible model lookup.
    pub(crate) async fn prepare_main_thread_agent_from_resume(
        &self,
        wanted: Option<String>,
        snapshot: Option<serde_json::Value>,
        target_model: &str,
        target_profile: Option<&str>,
    ) -> Result<PreparedMainThreadAgentRestore, agent::model_resolution::ModelResolutionError> {
        let mut resolved = match (wanted.as_deref(), snapshot) {
            (Some(wanted), Some(value)) => serde_json::from_value::<agent::AgentDefinition>(value)
                .ok()
                .filter(|definition| definition.agent_type == wanted),
            _ => None,
        };

        if resolved.is_none() {
            if let (Some(wanted), Some(catalog)) = (
                wanted.as_deref(),
                self.lifecycle_runtime.agent_catalog.as_ref(),
            ) {
                let catalog = catalog.read().await;
                // CC 2.1.218 resolves the resumed agentType by EXACT equality
                // only — no bare-name/suffix fallback (that belongs to the
                // `--agent` startup surface, not resume).
                resolved = catalog
                    .iter()
                    .find(|definition| definition.agent_type == wanted)
                    .cloned();
            }
        }

        let mut model_override = None;
        let mut hooks_trusted = false;
        if let Some(definition) = resolved.as_ref() {
            // (cc 2.1.218 `mvo`) ORIGIN TRUST — the RESUME surface. The
            // resumed `agent-setting` snapshot carries the definition's
            // `frontmatter_hooks` verbatim, so without this check a
            // definition whose hooks were correctly REFUSED at `--agent`
            // time would be silently installed on the next resume of that
            // session. Evaluated BEFORE the fields are moved below.
            hooks_trusted = agent::hooks_trust::agent_hooks_origin_trusted(&definition, &self.cwd);
            // (gap218 #43 / cc 2.1.218 `NQe`) Adopt the resumed agent's
            // frontmatter `model`, resolved to a wire id — the hot-resume twin
            // of the composition root's COLD-resume gate. Applied ONLY when the
            // user did NOT pass `--model` (`apply_resumed_agent_model` is
            // `!default_model_explicit`, set at the root) AND the agent declares
            // a concrete model (`AgentModel != Inherit`, oracle `i.model &&
            // i.model!=="inherit"`); an explicit `--model` is never overridden.
            // Evaluated BEFORE `definition.*` moves into the call below.
            model_override = if self.config.apply_resumed_agent_model {
                match &definition.model {
                    agent::AgentModel::Alias(spec) | agent::AgentModel::Explicit(spec) => {
                        let provider = self.model_resolution_context_provider.as_ref().ok_or_else(
                            || agent::model_resolution::ModelResolutionError::RouteUnavailable {
                                model: spec.clone(),
                                profile: target_profile.map(str::to_owned),
                                reason: "model route resolver is unavailable".into(),
                            },
                        )?;
                        // The saved model may have left the catalog. The
                        // agent's preference owns the next model selection;
                        // only its relative aliases need this profile's defaults.
                        let context = agent::model_resolution::ModelResolutionContext {
                            route: agent::model_resolution::ModelRouteFacts {
                                model: target_model.to_owned(),
                                profile: target_profile.map(str::to_owned),
                                ..Default::default()
                            },
                            ..Default::default()
                        };
                        let selection = agent::model_resolution::resolve_user_model_selection(
                            spec,
                            None,
                            &context,
                            provider.as_ref(),
                        )?;
                        Some(selection)
                    }
                    agent::AgentModel::Inherit => None,
                }
            } else {
                None
            };
        }
        Ok(PreparedMainThreadAgentRestore {
            wanted,
            definition: resolved,
            model_override,
            hooks_trusted,
        })
    }

    /// Apply a previously prepared agent after session activation.
    pub(crate) async fn restore_main_thread_agent_from_resume(
        &self,
        prepared: PreparedMainThreadAgentRestore,
    ) {
        let PreparedMainThreadAgentRestore {
            wanted,
            definition,
            model_override,
            hooks_trusted,
        } = prepared;
        match definition {
            Some(definition) => {
                if !hooks_trusted {
                    agent::hooks_trust::report_untrusted_hooks(
                        &definition,
                        &self.cwd,
                        agent::hooks_trust::HooksTrustSurface::MainThread,
                        false,
                    );
                }
                if let Some(selection) = model_override {
                    {
                        let mut state = self.session.lock().await;
                        state.model.clone_from(&selection.model);
                        state.model_profile.clone_from(&selection.model_profile);
                    }
                    self.tools
                        .set_main_loop_model(super::tooling_impl::canonical_main_loop_model(
                            &selection.model,
                            Some(&selection.model_resolution_context),
                        ));
                }
                self.set_main_thread_agent(
                    definition.agent_type,
                    definition.system_prompt,
                    definition.tools,
                    definition.disallowed_tools,
                    None,
                )
                .await;
                if hooks_trusted {
                    self.replace_main_thread_agent_hooks(&definition.frontmatter_hooks)
                        .await;
                } else {
                    // `QEt`'s untrusted arm ends in `b1r(void 0)` — it CLEARS the
                    // main-thread bucket rather than leaving it alone. Skipping
                    // the clear would strand the PREVIOUS session's hooks: an
                    // in-place resume from a trusted folder A into an untrusted
                    // session B would keep A's hook commands firing under B.
                    self.replace_main_thread_agent_hooks(&[]).await;
                }
            }
            None => {
                if let Some(wanted) = wanted {
                    tracing::warn!(
                        "Resumed session had agent \"{wanted}\" but it is no longer available. Using default behavior."
                    );
                }
                *self.lifecycle_runtime.main_thread_agent.write().await = None;
                self.replace_main_thread_agent_hooks(&[]).await;
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn restore_main_thread_agent_for_test(
        &self,
        wanted: Option<String>,
        snapshot: Option<serde_json::Value>,
    ) -> Result<(), agent::model_resolution::ModelResolutionError> {
        let (model, profile) = {
            let state = self.session.lock().await;
            (state.model.clone(), state.model_profile.clone())
        };
        let prepared = self
            .prepare_main_thread_agent_from_resume(wanted, snapshot, &model, profile.as_deref())
            .await?;
        self.restore_main_thread_agent_from_resume(prepared).await;
        Ok(())
    }

    /// The adopted main-thread agent's `agentType` (claude-code `MB()`), or
    /// `None` when no `--agent` was applied. Threaded into main-thread lifecycle
    /// hook payloads.
    pub(crate) async fn main_thread_agent_type(&self) -> Option<String> {
        self.lifecycle_runtime
            .main_thread_agent
            .read()
            .await
            .as_ref()
            .map(|a| a.agent_type.clone())
    }

    /// The system prompt for the next query, applying claude-code `nre`
    /// precedence: `overrideSystemPrompt` (`--system-prompt`) wins; else the
    /// adopted main-thread agent's prompt (`mainThreadAgentDefinition`
    /// `.getSystemPrompt()`); else the freshly assembled default.
    pub(crate) async fn effective_system_prompt(
        &self,
    ) -> Result<String, lingxi_core::settings::SettingsError> {
        self.effective_system_prompt_source()
            .await?
            .string_at("")
            .map(str::to_owned)
            .ok_or(lingxi_core::types::utf16_json::Utf16JsonProjectionError::InvalidPointer.into())
    }

    pub(crate) async fn effective_system_prompt_source(
        &self,
    ) -> Result<
        lingxi_core::types::utf16_json::Utf16JsonProjection,
        lingxi_core::settings::SettingsError,
    > {
        Ok(Self::system_prompt_source_text(
            &self.provider_system_prompt().await?,
        ))
    }

    /// Preserve the source vector through SDK selection and provider egress.
    /// Native snapshots the static array before separately appending the
    /// current query's dynamic system context.
    pub(crate) async fn provider_system_prompt(
        &self,
    ) -> Result<
        lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        lingxi_core::settings::SettingsError,
    > {
        if let Some(custom) = &self.config.system_prompt_override {
            *self
                .prompt_runtime
                .pending_prompt_source_vector
                .lock()
                .await = None;
            let mut units = custom
                .string_units("")
                .expect("system prompt override must be a JSON string");
            if let Some(append) = self
                .config
                .append_system_prompt
                .as_ref()
                .and_then(|source| source.string_units(""))
                .filter(|units| !units.is_empty())
            {
                if !units.is_empty() {
                    units.extend("\n\n".encode_utf16());
                }
                units.extend(append);
            }
            return Ok(lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::custom_prompt(
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_utf16(units),
            ));
        }
        let prompt = self.effective_system_prompt_parts().await?;
        let system_prompt = match prompt.snapshot_system_prompt.clone() {
            Some(snapshot) => snapshot,
            None if !prompt.source_sections.is_empty() => {
                let (model, profile) = self.current_prompt_route().await;
                let sections = prompt
                    .source_sections
                    .iter()
                    .map(|section| {
                        lingxi_llm_client::providers::anthropic::system_prompt::SourceSection {
                            text: section.text_prompt(),
                            scope: match section.scope {
                                crate::prompt::PromptSectionScope::Shared => {
                                    lingxi_llm_client::providers::anthropic::system_prompt::SectionScope::Shared
                                }
                                crate::prompt::PromptSectionScope::Session => {
                                    lingxi_llm_client::providers::anthropic::system_prompt::SectionScope::Session
                                }
                            },
                        }
                    })
                    .collect::<Vec<_>>();
                self.api
                    .prompt_snapshot_source_vector(&model, profile.as_deref(), &sections)
            }
            None => Vec::new(),
        };
        let dynamic_context = prompt
            .dynamic_system_context
            .map(lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string);
        let input = lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::source_vector(
            system_prompt.clone(),
            dynamic_context,
            Some(lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(
                crate::prompt::locked_templates::HEADER,
            )),
        );
        let pending = self
            .prompt_snapshot_eligible()
            .then_some(system_prompt)
            .filter(|vector| !vector.is_empty());
        *self
            .prompt_runtime
            .pending_prompt_source_vector
            .lock()
            .await = pending;
        Ok(input)
    }

    pub(super) async fn current_prompt_route(&self) -> (String, Option<String>) {
        if let Some(settings) = crate::scheduled_turn::current() {
            return (settings.model, Some(settings.provider));
        }
        if let Some(route) = crate::query_model::current() {
            return (route.model, route.profile);
        }
        let session = self.session.lock().await;
        (session.model.clone(), session.model_profile.clone())
    }

    async fn effective_system_prompt_parts(
        &self,
    ) -> Result<crate::prompt::RenderedPromptSections, lingxi_core::settings::SettingsError> {
        use crate::prompt::RenderedPromptSections;
        if let Some(custom) = &self.config.system_prompt_override {
            let display = custom
                .string_at("")
                .expect("system prompt override must be a JSON string")
                .to_owned();
            return Ok(RenderedPromptSections {
                text: display.clone(),
                static_text: display,
                source_sections: Vec::new(),
                snapshot_system_prompt: None,
                dynamic_system_context: None,
            });
        }
        if self.prompt_snapshot_eligible() {
            // Context routing also reads this slot. Drop the guard before
            // awaiting dynamic context; an if-let temporary retains it through
            // the entire body and would re-enter the same mutex.
            let snapshot = { self.prompt_runtime.prompt_snapshot.lock().await.clone() };
            if let Some(snapshot) = snapshot {
                if valid_prompt_snapshot(&snapshot) {
                    // Carved-slate freezes only the static getSystemPrompt
                    // members. `systemContext` (currently the git-status block
                    // in this port) is intentionally recomputed for every
                    // request, so a cwd/context change never gets baked into
                    // the cacheable prefix.
                    let snapshot_system_prompt = snapshot
                        .system_prompt
                        .iter()
                        .enumerate()
                        .map(|(index, section)| {
                            snapshot
                                .system_prompt_utf16
                                .get(index)
                                .and_then(Option::as_ref)
                                .map_or_else(
                                    || {
                                        lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(section.clone())
                                    },
                                    |units| {
                                        lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_utf16(units.clone())
                                    },
                                )
                        })
                        .collect::<Vec<_>>();
                    let static_text = snapshot_system_prompt
                        .iter()
                        .filter(|section| {
                            !section.equals_ascii(
                                lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY,
                            )
                        })
                        .map(|section| section.display_text())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    let mut prompt = RenderedPromptSections {
                        text: static_text.clone(),
                        static_text,
                        source_sections: Vec::new(),
                        snapshot_system_prompt: Some(snapshot_system_prompt),
                        dynamic_system_context: None,
                    };
                    prompt.dynamic_system_context =
                        self.append_dynamic_system_context(&mut prompt.text).await;
                    return Ok(prompt);
                }
            }
        }
        let main_thread_prompt = self
            .lifecycle_runtime
            .main_thread_agent
            .read()
            .await
            .as_ref()
            .and_then(|agent| agent.system_prompt.clone());
        let is_default_prompt = main_thread_prompt.is_none();
        let mut prompt = match main_thread_prompt {
            Some(source) => {
                let text = source
                    .string_at("")
                    .expect("main-thread agent prompt must be a JSON string")
                    .to_owned();
                let code_units = source
                    .string_units("")
                    .expect("main-thread agent prompt must be a JSON string");
                RenderedPromptSections {
                    static_text: text.clone(),
                    text: text.clone(),
                    source_sections: vec![crate::prompt::PromptSection {
                        id: "main_thread_agent".to_owned(),
                        text,
                        scope: crate::prompt::PromptSectionScope::Session,
                        id_utf16_code_units: None,
                        text_utf16_code_units: Some(code_units),
                    }],
                    snapshot_system_prompt: None,
                    dynamic_system_context: None,
                }
            }
            None => self.build_static_system_prompt_parts().await?,
        };
        if let Ok(profile) = self.prompt_runtime.app_agent_prompt_profile.read() {
            if let Some(profile) = profile.as_ref() {
                if !profile.instructions.trim().is_empty() {
                    let profile_section = format!(
                        "# App Agent Profile\nRevision: {}\n{}",
                        profile.revision, profile.instructions
                    );
                    prompt.text.push_str("\n\n");
                    prompt.text.push_str(&profile_section);
                    prompt.static_text.push_str("\n\n");
                    prompt.static_text.push_str(&profile_section);
                    prompt.source_sections.push(crate::prompt::PromptSection {
                        id: "app_agent_profile".to_owned(),
                        text: profile_section,
                        scope: crate::prompt::PromptSectionScope::Session,
                        id_utf16_code_units: None,
                        text_utf16_code_units: None,
                    });
                }
            }
        }
        self.append_native_append_system_prompt(&mut prompt);
        if is_default_prompt {
            prompt.dynamic_system_context =
                self.append_dynamic_system_context(&mut prompt.text).await;
        }
        Ok(prompt)
    }

    fn append_native_append_system_prompt(
        &self,
        prompt: &mut crate::prompt::RenderedPromptSections,
    ) {
        let Some(source) = self.config.append_system_prompt.as_ref() else {
            return;
        };
        append_source_system_prompt(prompt, source);
    }

    /// Oracle `lje(e)` (`src_169588164.js`) — whether the static-system-prompt
    /// snapshot is active for this session:
    ///
    /// ```js
    /// function lje(e){
    ///   if (e.systemPromptSnapshot === false) return false;
    ///   return e.systemPromptSnapshot === true
    ///       || a.CLAUDE_CODE_SESSION_KIND === "bg"
    ///       || a.CLAUDE_CODE_REMOTE
    ///       || !U2();                       // U2() = a.CLAUDE_CODE_SIMPLE
    /// }
    /// ```
    ///
    /// An explicit `--system-prompt-snapshot off` overrules every disjunct;
    /// `on` forces it regardless of session kind. With the flag absent the
    /// default is `!simple`, i.e. ON — 2.1.252's `tengu_carved_slate` rollout
    /// flag GRADUATED and no longer exists anywhere in the 2.1.270 binary
    /// (verified: 2347 distinct `tengu_` flags present, that one absent). The
    /// port had kept requiring it, so this whole recorded-prompt feature —
    /// implemented, tested, and restored on resume — could never switch on.
    ///
    /// `CLAUDE_CODE_REMOTE` is deliberately omitted: remote surfaces are out of
    /// scope for this port, and it is a pure disjunct, so the only behaviour it
    /// could add is for a session that is remote AND simple.
    pub(crate) fn snapshot_gate(
        explicit: Option<bool>,
        background_session: bool,
        simple_mode: bool,
    ) -> bool {
        if explicit == Some(false) {
            return false;
        }
        explicit == Some(true) || background_session || !simple_mode
    }

    /// Whether the static-system-prompt snapshot gate is active for this main
    /// conversation. Auxiliary query sources and custom prompts are excluded —
    /// upstream applies those at the recording seam (`swt`), and the port
    /// carries them here because one predicate feeds both record and reuse.
    pub(crate) fn prompt_snapshot_eligible(&self) -> bool {
        let simple = lingxi_core::host::env::is_env_truthy(
            std::env::var(branding::SIMPLE_ENV).ok().as_deref(),
        );
        // `CLAUDE_CODE_SESSION_KIND` in claude-code; the port already spells it
        // `LINGXI_SESSION_KIND` at its other reader (`tool_api::defer`).
        let background_session = std::env::var("LINGXI_SESSION_KIND").ok().as_deref() == Some("bg");
        let source = crate::config::sanitize_query_source(&self.config.query_source);
        Self::snapshot_gate(
            lingxi_core::host::session_flags::system_prompt_snapshot(),
            background_session,
            simple,
        ) && self.config.system_prompt_override.is_none()
            && source != "auxiliary"
            && !source.starts_with("auxiliary:")
    }

    /// The resume path must never manufacture a missing snapshot. This flag is
    /// set by cold/hot resume restoration and cleared by `/clear`.
    pub(crate) fn prompt_snapshot_resume(&self) -> bool {
        self.prompt_runtime
            .prompt_snapshot_resume
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Persist the first eligible request's static prompt and initial inline
    /// tool descriptions. State is published before disk I/O so an in-memory
    /// host still gets stable semantics when persistence is unavailable.
    pub(crate) async fn record_prompt_snapshot_if_needed(
        &self,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        wire_tools: &[lingxi_core::host::SourceToolWireDefinition],
    ) {
        if !self.prompt_snapshot_eligible()
            || self.prompt_snapshot_resume()
            || system.is_none_or(|prompt| prompt.display_text().is_empty())
        {
            return;
        }
        let Some(system_prompt) = self
            .prompt_runtime
            .pending_prompt_source_vector
            .lock()
            .await
            .take()
            .filter(|vector| !vector.is_empty())
        else {
            return;
        };
        let system_prompt_utf16 = system_prompt
            .iter()
            .map(|text| {
                let units = text.utf16_code_units().to_vec();
                String::from_utf16(&units).is_err().then_some(units)
            })
            .collect::<Vec<_>>();
        let system_prompt = system_prompt
            .iter()
            .map(|text| text.display_text().to_owned())
            .collect::<Vec<_>>();
        let snapshot = lingxi_core::host::PromptSnapshot {
            system_prompt,
            system_prompt_utf16,
            tools: prompt_tool_descriptions(wire_tools),
            context_rendering: Some(self.current_context_rendering().await),
        };
        let mut slot = self.prompt_runtime.prompt_snapshot.lock().await;
        if slot.is_some() {
            return;
        }
        *slot = Some(snapshot.clone());
        drop(slot);
        self.persist_prompt_snapshot_attachment(&snapshot).await;
    }

    /// After a successful, non-API-error response, append a replacement
    /// snapshot only for newly seen non-deferred inline tools. Existing names
    /// retain their original descriptions and order forever.
    pub(crate) async fn record_inline_prompt_tools_after_success(
        &self,
        wire_tools: &[lingxi_core::host::SourceToolWireDefinition],
    ) {
        if !self.prompt_snapshot_eligible() || self.prompt_snapshot_resume() {
            return;
        }
        let additions = prompt_tool_descriptions(wire_tools);
        if additions.is_empty() {
            return;
        }
        let snapshot = {
            let mut slot = self.prompt_runtime.prompt_snapshot.lock().await;
            let Some(snapshot) = slot.as_mut() else {
                return;
            };
            let known: std::collections::HashSet<String> = snapshot
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect();
            let mut added = false;
            for tool in additions {
                if !known.contains(&tool.name) {
                    snapshot.tools.push(tool);
                    added = true;
                }
            }
            added.then(|| snapshot.clone())
        };
        if let Some(snapshot) = snapshot {
            self.persist_prompt_snapshot_attachment(&snapshot).await;
        }
    }

    async fn persist_prompt_snapshot_attachment(
        &self,
        snapshot: &lingxi_core::host::PromptSnapshot,
    ) {
        use lingxi_core::types::utf16_json::Utf16JsonProjection;

        if snapshot.system_prompt_utf16.len() > snapshot.system_prompt.len() {
            tracing::warn!("discarding prompt snapshot with unmatched exact UTF-16 strings");
            return;
        }
        let system_prompt = snapshot
            .system_prompt
            .iter()
            .enumerate()
            .map(|(index, display)| {
                let units = snapshot
                    .system_prompt_utf16
                    .get(index)
                    .and_then(Option::as_deref)
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| display.encode_utf16().collect());
                Utf16JsonProjection::root_string(display.clone(), units)
            })
            .collect::<Result<Vec<_>, _>>()
            .and_then(Utf16JsonProjection::array);
        let projection = system_prompt.and_then(|system_prompt| {
            let mut fields = vec![
                (
                    "type".to_string(),
                    Utf16JsonProjection::root_string(
                        "prompt_snapshot".to_string(),
                        "prompt_snapshot".encode_utf16().collect(),
                    )?,
                ),
                ("systemPrompt".to_string(), system_prompt),
            ];
            if !snapshot.tools.is_empty() {
                let tools_source = serde_json::to_string(&snapshot.tools)
                    .map_err(lingxi_core::types::utf16_json::Utf16JsonProjectionError::Serialize)?;
                let tools = Utf16JsonProjection::parse(&tools_source)?;
                fields.push(("tools".to_string(), tools));
            }
            if let Some(rendering) = snapshot.context_rendering {
                let rendering = serde_json::to_value(rendering).map_err(|error| {
                    lingxi_core::types::utf16_json::Utf16JsonProjectionError::Serialize(error)
                })?;
                fields.push((
                    "contextRendering".to_string(),
                    Utf16JsonProjection::from_serde_value(&rendering)?,
                ));
            }
            Utf16JsonProjection::object(fields)
        });
        match projection {
            Ok(projection) => self.persist_hook_attachment_to_jsonl(projection).await,
            Err(error) => tracing::warn!(%error, "discarding invalid prompt snapshot projection"),
        }
    }

    /// Install a host-approved app Agent Profile as an additive prompt layer.
    /// The next turn observes it; the current in-flight request keeps its
    /// already-built prompt.
    pub fn set_app_agent_prompt_profile(
        &self,
        revision: u64,
        instructions: String,
    ) -> Result<(), String> {
        if instructions.len() > 32 * 1024 {
            return Err("app Agent Profile exceeds 32 KiB".into());
        }
        let mut profile = self
            .prompt_runtime
            .app_agent_prompt_profile
            .write()
            .map_err(|_| "app Agent Profile lock is poisoned".to_string())?;
        if profile
            .as_ref()
            .is_some_and(|current| revision < current.revision)
        {
            return Err("app Agent Profile revision moved backwards".into());
        }
        *profile = Some(AppAgentPromptProfile {
            revision,
            instructions,
        });
        Ok(())
    }

    /// Remove the app-specific additive prompt layer when an app session is
    /// closed or the host switches back to the ordinary conversation.
    pub fn clear_app_agent_prompt_profile(&self) -> Result<(), String> {
        self.prompt_runtime
            .app_agent_prompt_profile
            .write()
            .map_err(|_| "app Agent Profile lock is poisoned".to_string())?
            .take();
        Ok(())
    }

    /// Read-only introspection seam for the ordinary per-turn additional
    /// context (`instructions` / `userEmail` / `currentDate`). Companion to
    /// [`Self::assemble_system_prompt_preview`] — lets a host/composition-root
    /// test prove an injected memory provider reaches the additional-context
    /// message without a live model round-trip.
    pub async fn additional_context_preview(
        &self,
    ) -> Result<Option<String>, lingxi_core::settings::SettingsError> {
        let Some(message) = self.additional_context_message().await? else {
            return Ok(None);
        };
        let preview = match message {
            ConversationMessage::User { content, .. } => {
                content.into_iter().find_map(|block| match block {
                    lingxi_core::types::ContentBlock::Text { text, .. }
                    | lingxi_core::types::ContentBlock::TextJsUtf16 { text, .. } => Some(text),
                    _ => None,
                })
            }
            _ => None,
        };
        Ok(preview)
    }

    /// Prepend the fixed runtime context and, for inline routing, userContext.
    /// Normal routing's typed announcements are durable history messages.
    pub(crate) async fn prepend_leading_context(
        &self,
        messages: &mut Vec<ConversationMessage>,
        context: &PreparedContextAnnouncements,
    ) {
        if let Some(message) = &context.inline_context {
            messages.retain(|existing| existing.id() != message.id());
            messages.insert(0, message.clone());
        }
        if let Some(workspace) = self.mobile_workspace_environment_message() {
            messages.insert(0, workspace);
        }
        if let Some(runtime) = self.mobile_runtime_environment_message().await {
            messages.insert(0, runtime);
        }
    }

    fn mobile_workspace_environment_message(&self) -> Option<ConversationMessage> {
        let environment = self.mobile_runtime_environment.as_ref()?;
        let cwd = self.session_cwd.cwd();
        let model_cwd = match &self.mobile_workspace_cwd_resolver {
            Some(resolver) => resolver(&cwd),
            None => Some(cwd.to_string_lossy().into_owned()),
        };
        let reminder = environment.render_workspace_system_reminder(model_cwd.as_deref())?;
        Some(ConversationMessage::user_meta(MessageId::new(), reminder))
    }

    pub(crate) fn prompt_is_interactive(&self) -> bool {
        self.mobile_runtime_environment.as_ref().map_or(
            self.config.interactive_session,
            |environment| {
                !matches!(
                    environment.host.launch_mode,
                    lingxi_core::host::MobileLaunchMode::ScheduledHeadless
                )
            },
        )
    }

    /// Prepend a transient call-scoped reminder without displacing the fixed
    /// mobile runtime snapshot from index zero.
    ///
    /// Desktop callers retain the historical index-zero behavior. Mobile
    /// callers place date/deferred-tool deltas immediately after the fixed
    /// runtime reminder, keeping that cache-stable prefix in one position on
    /// initial, retry, and fallback requests.
    pub(crate) fn prepend_transient_leading_context(
        &self,
        messages: &mut Vec<ConversationMessage>,
        reminder: ConversationMessage,
    ) {
        let mut index = usize::from(messages.first().is_some_and(|message| {
            Self::is_mobile_runtime_environment_message(message)
                || self
                    .mobile_runtime_environment_message
                    .as_ref()
                    .is_some_and(|runtime| runtime == message)
        }));
        if self.mobile_runtime_environment.is_some()
            && messages.get(index).is_some_and(|message| {
                matches!(message, ConversationMessage::User { content, .. } if content.iter().any(
                    |block| matches!(block, lingxi_core::types::ContentBlock::Text { text, .. } if text.starts_with("<system-reminder>\nMobile workspace context"))
                ))
            })
        {
            index += 1;
        }
        messages.insert(index, reminder);
    }

    /// Native's provider projection reorders typed attachments throughout the
    /// history, including earlier turns. Keep durable append order unchanged so
    /// live and cold-resumed requests derive the same stable physical prefix.
    pub(crate) async fn project_typed_attachment_order(
        &self,
        messages: &mut Vec<ConversationMessage>,
        turn_reminders: &[ConversationMessage],
        context_announcements: &[ConversationMessage],
        model_supports_mid_conversation_system: bool,
    ) {
        let attachments = self
            .transcript
            .model_reminder_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut typed_ids = attachments
            .keys()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        // Hook attachments retain their raw JSONL row separately from model
        // reminders. Their registered identities still participate in zjr;
        // otherwise a live UPE row stays after its command while cold restore
        // correctly places the same attachment before it.
        let persisted_attachment_types = self
            .prompt_runtime
            .mod_persisted_attachments
            .lock()
            .await
            .iter()
            .map(|(id, (kind, _))| (*id, kind.clone()))
            .collect::<std::collections::HashMap<_, _>>();
        typed_ids.extend(persisted_attachment_types.keys().copied());
        typed_ids.extend(
            turn_reminders
                .iter()
                .chain(context_announcements)
                .map(ConversationMessage::id),
        );
        let snapshot_ids = messages
            .iter()
            .map(ConversationMessage::id)
            .collect::<std::collections::HashSet<_>>();
        let (marked_ids, trusted_zce_ids) = {
            let session = self.session.lock().await;
            (
                session
                    .native_request_history
                    .before_closing_line_ids
                    .clone(),
                session.native_request_history.synthetic_closing_ids.clone(),
            )
        };
        let mut raw_attachment_types = attachments
            .iter()
            .filter_map(|(id, payload)| {
                payload
                    .string_at("/type")
                    .map(|attachment_type| (*id, attachment_type.to_owned()))
            })
            .collect::<std::collections::HashMap<_, _>>();
        for (id, kind) in persisted_attachment_types {
            raw_attachment_types.entry(id).or_insert(kind);
        }
        let eligible_marked_ids = marked_ids
            .into_iter()
            .filter(|id| {
                snapshot_ids.contains(id)
                    && super::closing_placement::is_typed_tee_attachment(
                        *id,
                        &typed_ids,
                        &raw_attachment_types,
                    )
            })
            .collect::<std::collections::HashSet<_>>();
        *self
            .transcript
            .request_history_before_attachment_projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = messages.clone();
        let task_delivery_ids = self
            .task_deliveries_for_snapshot(messages)
            .await
            .into_keys()
            .collect::<std::collections::HashSet<_>>();
        let closing_projection = model_supports_mid_conversation_system.then_some(
            super::closing_placement::ClosingPlacementProjection {
                eligible_marked_ids: &eligible_marked_ids,
                trusted_zce_ids: &trusted_zce_ids,
            },
        );
        super::attachment_order::repack_typed_attachments(
            messages,
            &typed_ids,
            Some(&task_delivery_ids),
            closing_projection,
        );

        // Typed identity comes from the attachment sidecar, regardless of the
        // user row's `is_meta` flag. Replace the execution-only origin set on
        // every projection so compacted or expired transient rows cannot leak
        // into a later request.
        let request_ids = typed_attachment_ids_in_snapshot(&typed_ids, messages);
        *self
            .transcript
            .request_attachment_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = request_ids;
    }

    /// Return typed attachment ids present in this exact request snapshot.
    /// `is_meta` does not determine attachment identity. Durable sidecars
    /// reconstruct historical origins after cold resume; the execution-only
    /// set covers current rendered origins whose payload remains staged or
    /// otherwise transient.
    pub(crate) fn typed_attachment_ids_for_snapshot(
        &self,
        messages: &[ConversationMessage],
    ) -> std::collections::HashSet<lingxi_core::types::MessageId> {
        let mut typed_ids = self
            .transcript
            .model_reminder_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        typed_ids.extend(
            self.transcript
                .request_attachment_ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .copied(),
        );
        typed_attachment_ids_in_snapshot(&typed_ids, messages)
    }

    /// Reattach all call-scoped context after rebuilding a request from raw
    /// session history (for example after a context-overflow retry).
    pub(crate) async fn reattach_outgoing_context(
        &self,
        messages: &mut Vec<ConversationMessage>,
        deferred_tools_reminder: Option<&ConversationMessage>,
        date_change_reminder: Option<&ConversationMessage>,
        turn_reminders: &mut Vec<ConversationMessage>,
        guarded_async_hook_reminders: &mut Vec<(
            lingxi_core::types::MessageId,
            std::sync::Arc<dyn hooks::attachment::HookPublicationGuard>,
        )>,
        context_announcements: &PreparedContextAnnouncements,
        after_compaction: bool,
    ) {
        crate::prompt::async_hook_response::retain_current_async_hook_reminders(
            messages,
            turn_reminders,
            guarded_async_hook_reminders,
        );
        // Preparation persisted MCP/total-token attachments into history while
        // placing them among transient reminders in the initial request. Remove
        // this step's copies before restoring the same ordered suffix on retry.
        // Older attachments and durable task completions retain their positions.
        let reminder_ids = turn_reminders
            .iter()
            .map(ConversationMessage::id)
            .collect::<std::collections::HashSet<_>>();
        messages.retain(|message| !reminder_ids.contains(&message.id()));
        messages.extend(turn_reminders.iter().cloned());
        let current_context_announcements = if after_compaction {
            let current = self
                .context_announcements_from_frozen_with_reason(context_announcements.refresh_reason)
                .await;
            messages.extend(current.iter().cloned());
            current
        } else {
            context_announcements.reattach(messages);
            context_announcements.messages.clone()
        };
        crate::request_context::attach(messages);
        let (model, profile) = {
            let session = self.session.lock().await;
            (session.model.clone(), session.model_profile.clone())
        };
        let model_supports_mid_conversation_system = self
            .api
            .supports_native_mid_conversation_system(&model, profile.as_deref());
        self.project_typed_attachment_order(
            messages,
            turn_reminders,
            &current_context_announcements,
            model_supports_mid_conversation_system,
        )
        .await;
        self.prepend_leading_context(messages, context_announcements)
            .await;
        if let Some(reminder) = deferred_tools_reminder {
            self.prepend_transient_leading_context(messages, reminder.clone());
        }
        if let Some(reminder) = date_change_reminder {
            self.prepend_transient_leading_context(messages, reminder.clone());
        }
        self.screen_mod_persisted_attachments(messages).await;
    }

    /// Read-only test/host preview of the fixed runtime reminder.
    pub async fn mobile_runtime_environment_preview(&self) -> Option<String> {
        self.mobile_runtime_environment_message()
            .await
            .and_then(|message| Self::text_content(&message))
    }

    /// PathAtlas S3: map the model-visible session cwd to the directory the
    /// prompt-side filesystem probes must actually read.
    ///
    /// On mobile the session cwd is a mobile-linux GUEST path
    /// (`engine-mobile`'s `model_cwd` comes from `workspace_mount.guest_path`),
    /// so probing it verbatim reads a directory that does not exist on the
    /// host. Desktop never installs a resolver, so `probe_cwd == cwd` there —
    /// byte-identical (INERT INVARIANT).
    ///
    /// Both prompt-side memory readers go through here: the system prompt
    /// ([`Self::build_prompt_context`]) and the per-turn `instructions` context
    /// message ([`Self::additional_context_message`]). Keeping ONE derivation
    /// is the point — the second reader having its own (raw, unresolved) copy
    /// is what kept every mobile workspace `LINGXI.md` out of the model.
    pub(crate) fn prompt_probe_cwd(&self, cwd: &std::path::Path) -> std::path::PathBuf {
        match &self.prompt_probe_cwd_resolver {
            Some(resolver) => resolver(cwd),
            None => cwd.to_path_buf(),
        }
    }

    /// Build the per-turn system prompt by gathering cwd / git / file
    /// tree / memory / tool-name context and calling
    /// [`crate::prompt::assemble_system_prompt`]. Bypassed when
    /// `OrchestratorConfig::system_prompt_override` is `Some(_)`.
    /// Build the per-turn [`crate::prompt::SystemPromptContext`] (cwd / env /
    /// git / tools / memory / …). Shared by [`Self::build_system_prompt`] and
    /// [`Self::additional_context_message`] — the latter re-emits the env block
    /// in the first user message when `--exclude-dynamic-system-prompt-sections`
    /// is set, so the construction (and its env-field probes) lives in ONE place.
    async fn build_prompt_context(
        &self,
    ) -> Result<crate::prompt::SystemPromptContext, lingxi_core::settings::SettingsError> {
        let memory_files = self
            .main_instruction_files()
            .await
            .map_err(|error| Self::source_settings_failure("instruction_context", error))?;
        Ok(self
            .build_prompt_context_from_files(memory_files.as_ref().clone())
            .await?)
    }

    async fn build_prompt_context_from_files(
        &self,
        memory_files: Vec<crate::prompt::MemoryFile>,
    ) -> Result<crate::prompt::SystemPromptContext, lingxi_core::settings::SettingsError> {
        use crate::prompt::{FileTree, SystemPromptContext};

        // Task 5 (worktree 206 session-cwd plumbing): read the LIVE
        // `self.session_cwd` — the SAME cell `EnterWorktree`/`ExitWorktree`
        // swap on the tool side — not the frozen `self.cwd`. The env block's
        // `Primary working directory:` line, the file tree, the git-status
        // probe, and the memory hierarchy below all derive from `cwd`, so this
        // one substitution re-derives the ENTIRE prompt context from the
        // post-swap worktree every turn. `self.session_cwd` defaults to a
        // private, never-swapped cell equal to `self.cwd` when
        // `with_session_cwd` was never called, so this is byte-identical to
        // before for every caller that doesn't wire it (INERT INVARIANT).
        let cwd = self.session_cwd.cwd();
        // PathAtlas S3: when the session cwd is a mobile-linux guest path,
        // the probes below (memory hierarchy, git status, file tree,
        // worktree check) must read the HOST directory backing it while the
        // env block's `Primary working directory:` keeps displaying the
        // guest path the model actually uses. Desktop never sets the
        // resolver, so `probe_cwd == cwd` there — byte-identical.
        let probe_cwd = self.prompt_probe_cwd(&cwd);
        // The `<env>` model-identity line ("You are powered by the model named
        // …") must reflect the CURRENT model, not the launch model. `/model`
        // switches update `session.model` (see `OrchestratorHandle::switch_model`
        // → `SessionState::model`), while `config.model` stays frozen at
        // startup. Reading `config.model` here froze the injected identity, so a
        // switched-to model (e.g. Fable 5) still saw "You are Opus 4.8" in its
        // system prompt and reported the stale identity. The outgoing REQUEST
        // model already re-snapshots `session.model` each turn; this aligns the
        // prompt identity with it. Locked briefly and released — every
        // `build_system_prompt` caller builds the prompt BEFORE taking the
        // session lock, so there is no reentrancy.
        let model = self.session.lock().await.model.clone();

        let (git, _) = self.cached_git_status(&probe_cwd).await;

        // Tool name extraction: ToolRegistry's `all_names()` is the
        // unfiltered set (builtin + plugin + MCP). M5-03 uses the
        // unfiltered list because the registry's enable-filter requires
        // a `ToolStaticContext` that's only meaningful at dispatch time.
        // tools_block::format sorts alphabetically inside.
        let tool_names: Vec<String> = self.tools.all_names();

        let shell = crate::prompt::env_meta::detect_shell();

        // DIV-1: worktree detection — `hf()!==null` in claude-code. Detect a
        // worktree by checking for the `gitdir` file that git creates in worktree
        // checkouts (a file rather than a directory at .git). Computed before `cwd`
        // is moved into the context struct below.
        let in_worktree = probe_cwd.join(".git").is_file();

        Ok(SystemPromptContext {
            cwd,
            // `Platform: ${je.platform}` — claude-code emits the node
            // `process.platform` value (`darwin`/`linux`/`win32`), NOT Rust's
            // `std::env::consts::OS` (`macos`/`linux`/`windows`). Map the two
            // divergent names so the env line is byte-exact.
            platform: node_platform_name(std::env::consts::OS).to_string(),
            // SYSPROMPT.1: port TS getMarketingNameForModel / getKnowledgeCutoff
            // (`utils/model/model.ts:570`, `constants/prompts.ts:712`) so the
            // model line + cutoff sentence match claude-code instead of being
            // stubbed to None. Sourced from the LIVE `session.model` (above) so
            // `/model` switches take effect for the identity block.
            //
            // NON-Claude fallback: `marketing_name_for_model` only names Claude
            // ids, so a switched-to non-Claude model (deepseek/gemini/…) got the
            // weak id-only "powered by the model {id}." form. Fall back to the
            // catalog display name so EVERY turn's identity line uses the strong
            // "powered by the model named {name}." form with the CURRENT model.
            // Gated on non-Claude so Claude ids keep byte-parity (an unknown
            // Claude id stays id-only exactly like claude-code).
            model_marketing_name: crate::prompt::env_meta::marketing_name_for_model(&model)
                .map(String::from)
                .or_else(|| {
                    (!model.to_ascii_lowercase().contains("claude"))
                        .then(|| crate::provider_adapter::display_name_for_model(&model))
                        .flatten()
                }),
            knowledge_cutoff: crate::prompt::env_meta::knowledge_cutoff_for_model(&model)
                .map(String::from),
            model,
            shell,
            // SYSPROMPT.1: `uname -sr` (TS getUnameSR) e.g. "Darwin 25.3.0",
            // falling back to "<os> <arch>" on Windows / spawn failure.
            os_version: crate::prompt::env_meta::os_version_string(),
            git_status: git,
            in_worktree,
            file_tree: FileTree::default(),
            memory_files,
            tool_names,
            // `nz()` non-empty: at least one model-invocable prompt skill
            // exists. Sourced from the attached skill-listing provider (same
            // source as the per-turn skill reminder); `None` provider ⇒ false.
            skills_available: match &self.prompt_runtime.skill_listing {
                Some(provider) => !provider.skill_listing_snapshot().await?.entries.is_empty(),
                None => false,
            },
            // A scheduled mobile runtime shares a process with the foreground
            // conversation, so `interactive_session` intentionally remains
            // process-interactive. The typed per-orchestrator launch mode is
            // authoritative for prompt guidance and avoids advertising `!`
            // commands or other live-UI actions to a headless Cron run.
            is_interactive: self.prompt_is_interactive(),
            // `# Memory` section gate (claude-code `tengu_moth_copse`, default
            // OFF): the memory feature is active iff a memory prefetch is wired
            // (`memory_prefetch.is_some()`), and the section points the model at
            // exactly the user memdir the prefetch scans. `None` ⇒ section
            // omitted (byte-identical to the pre-memory prompt).
            memory_dir: self
                .prompt_runtime
                .memory_prefetch
                .as_ref()
                .and_then(|p| p.user_memdir())
                .map(std::path::Path::to_path_buf),
            // `--exclude-dynamic-system-prompt-sections`: when set, `assemble`
            // OMITS the env block from the system prompt (it is re-emitted in the
            // first-user-message context reminder via `env_reminder_section`).
            exclude_dynamic_sections: self.config.exclude_dynamic_system_prompt_sections,
        })
    }

    /// The output style in force for THIS turn: a `/output-style` switch made
    /// during the session, else the boot setting.
    ///
    /// Read per turn rather than captured once, mirroring upstream, where the
    /// `/output-style` handler writes `outputStyle` into localSettings and the
    /// next prompt assembly reads it back. `Some("default")` is an explicit
    /// choice and must win over an inherited custom style — treating it as
    /// "unset" would make switching BACK to the default impossible.
    pub(crate) async fn active_output_style_setting(
        &self,
    ) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
        if let Some(refreshed) = self.prompt_runtime.live_output_style.lock().await.clone() {
            return refreshed;
        }
        self.config.output_style.clone()
    }

    /// Current model-facing response-language setting, retained as the exact
    /// source value used by `SettingsJson`.
    pub(crate) async fn active_response_language(
        &self,
    ) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
        self.prompt_runtime
            .live_response_language
            .lock()
            .await
            .clone()
    }

    /// Refresh the effective response language after watched user, project,
    /// local, or managed settings change.
    pub async fn refresh_native_response_language(
        &self,
        language: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) {
        *self.prompt_runtime.live_response_language.lock().await = language;
    }

    /// Refresh the effective output-style selection after settings change.
    pub async fn refresh_native_output_style(
        &self,
        output_style: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) {
        *self.prompt_runtime.live_output_style.lock().await = Some(output_style);
    }

    pub(super) async fn resolve_active_output_style(
        &self,
    ) -> Option<outputstyles::ResolvedOutputStyle> {
        let setting = self.active_output_style_setting().await;
        let setting_name = setting.as_ref().and_then(|source| {
            let units = source.string_units("")?;
            String::from_utf16(&units).ok()
        });
        if let Some(registry) = &self.prompt_runtime.output_style_registry {
            if let Some(style) = registry.read().await.resolve(setting_name.as_deref()) {
                return Some(style);
            }
        }
        outputstyles::resolve_output_style(setting_name.as_deref(), &self.config.output_style_dirs)
    }

    /// Assemble the full system-prompt STRING from the static prompt members
    /// plus the current dynamic `systemContext` block.
    /// Bypassed when `OrchestratorConfig::system_prompt_override` is `Some(_)`.
    #[cfg(test)]
    pub(super) async fn build_system_prompt(
        &self,
    ) -> Result<String, lingxi_core::settings::SettingsError> {
        Ok(self.build_system_prompt_parts().await?.text)
    }

    async fn build_system_prompt_parts(
        &self,
    ) -> Result<crate::prompt::RenderedPromptSections, lingxi_core::settings::SettingsError> {
        let mut prompt = self.build_static_system_prompt_parts().await?;
        prompt.dynamic_system_context = self.append_dynamic_system_context(&mut prompt.text).await;
        Ok(prompt)
    }

    /// Build only the cacheable `getSystemPrompt` members. The git-status
    /// suffix is kept out so a carved-slate snapshot can reuse this prefix
    /// while still receiving live dynamic system context on every request.
    async fn build_static_system_prompt_parts(
        &self,
    ) -> Result<crate::prompt::RenderedPromptSections, lingxi_core::settings::SettingsError> {
        use crate::prompt::{
            named_prompt_sections_with_style, prompt_section_slots, render_prompt_sections_scoped,
            ActiveOutputStyle, PromptSectionScope,
        };
        let ctx = self.build_prompt_context().await?;
        // OUTSTYLE.2/.3: when a non-default output style is active — a builtin
        // OR a custom disk style discovered under `output_style_dirs` — inject
        // its `# Output Style: <name>` section (TS getOutputStyleSection). A
        // `None`/`"default"`/unknown style resolves to `None`, leaving the prompt
        // byte-identical to the styleless path (empty `output_style_dirs` ⇒
        // builtin-only, as before).
        let resolved = self.resolve_active_output_style().await;
        let style = resolved.as_ref().map(|r| ActiveOutputStyle {
            name: r.name.as_str(),
            prompt: r.prompt.as_str(),
            keep_coding_instructions: r.keep_coding_instructions,
        });
        let host = if let Some(registry) = &self.lifecycle_runtime.hook_registry {
            registry.read().await.mod_host()
        } else {
            None
        };
        {
            let mut cache = self.prompt_runtime.mod_prompt_sections.lock().await;
            let context = (
                ctx.model.clone(),
                ctx.cwd.clone(),
                host.as_ref().map(|host| host.registration_identity()),
            );
            if cache.context.as_ref() != Some(&context) {
                cache.generation = cache.generation.wrapping_add(1);
                cache.context = Some(context);
                cache.answers.clear();
            }
        }
        let sections = named_prompt_sections_with_style(&ctx, style);
        let resolved_sections = if let Some(host) = host.as_ref() {
            futures::future::join_all(prompt_section_slots(sections, &ctx).into_iter().map(
                |slot| async move {
                    let id = slot.id_prompt();
                    let slot_text = slot.text_prompt();
                    let text =
                        if slot.scope == PromptSectionScope::Session && slot.id != "output_style" {
                            self.dispatch_mod_prompt_section(
                                Some(host),
                                &id,
                                slot_text.as_ref(),
                                true,
                                None,
                                None,
                            )
                            .await
                        } else {
                            slot_text
                        };
                    text.map(|text| {
                        let text_utf16_code_units = Some(text.utf16_code_units().to_vec());
                        crate::prompt::PromptSection {
                            id: slot.id,
                            text: text.display_text().to_owned(),
                            scope: slot.scope,
                            id_utf16_code_units: slot.id_utf16_code_units,
                            text_utf16_code_units,
                        }
                    })
                },
            ))
            .await
            .into_iter()
            .flatten()
            .collect()
        } else {
            sections
        };
        let composed = if let Some(host) = host.as_ref() {
            self.dispatch_mod_prompt_compose(host, &ctx, style, resolved_sections)
                .await
        } else {
            resolved_sections
        };
        Ok(render_prompt_sections_scoped(&composed))
    }

    async fn dispatch_mod_prompt_section(
        &self,
        host: Option<&std::sync::Arc<hooks::mods::ModHost>>,
        name: &lingxi_llm_client::providers::anthropic::system_prompt::PromptText,
        text: Option<&lingxi_llm_client::providers::anthropic::system_prompt::PromptText>,
        stored: bool,
        origin: Option<serde_json::Value>,
        skip_hook_id: Option<u64>,
    ) -> Option<lingxi_llm_client::providers::anthropic::system_prompt::PromptText> {
        let Some(host) = host else {
            return text.cloned();
        };
        let name_units = name.utf16_code_units().to_vec();
        let source_units = text.map(|text| text.utf16_code_units().to_vec());
        let generation = if stored {
            let cache = self.prompt_runtime.mod_prompt_sections.lock().await;
            if let Some((source, answer)) = cache.answers.get(&name_units) {
                if source.as_ref() == source_units.as_ref() {
                    return answer.clone();
                }
            }
            Some(cache.generation)
        } else {
            None
        };
        let public_name = public_prompt_name(name);
        let input = projection_object(vec![
            ("name".into(), prompt_text_projection(&public_name)),
            (
                "text".into(),
                text.map(prompt_text_projection)
                    .unwrap_or_else(|| Utf16JsonProjection::parse("null").unwrap()),
            ),
        ]);
        let pinned_name = public_name.utf16_code_units().to_vec();
        let log_output = self.output.clone();
        let toast_output = self.output.clone();
        let status_output = self.output.clone();
        let cwd = self.session_cwd.cwd();
        let api_origin = origin.as_ref().map_or(
            lingxi_core::host::task_registry::FieldPresence::Missing,
            |origin| lingxi_core::host::task_registry::FieldPresence::Value(origin.clone()),
        );
        let result = host
            .dispatch_with_utf16_at_context(
                "prompt.section",
                input,
                &cwd,
                Some(self),
                None,
                origin,
                skip_hook_id,
                api_origin,
                move |event| {
                    let pinned_name = pinned_name.clone();
                    async move {
                        let Some(event) = event else {
                            return Err(hooks::mods::ModError::Hook(
                                "prompt.section needs a forwarded event".into(),
                            ));
                        };
                        if event.string_units("/name").as_deref() != Some(pinned_name.as_slice()) {
                            return Err(hooks::mods::ModError::Hook(
                                "prompt.section name is pinned".into(),
                            ));
                        }
                        let text = event
                            .subprojection("/text")
                            .unwrap_or_else(|_| Utf16JsonProjection::parse("null").unwrap());
                        Ok(Some(projection_object(vec![("text".into(), text)])))
                    }
                },
                move |plugin, text| {
                    let output = log_output.clone();
                    async move { output.emit_mod_log(&plugin, &text).await }
                },
                move |plugin, text, timeout_ms| {
                    let output = toast_output.clone();
                    async move { output.emit_mod_toast(&plugin, &text, timeout_ms).await }
                },
                move |plugin, text| {
                    let output = status_output.clone();
                    async move { output.emit_mod_status(&plugin, text.as_ref()).await }
                },
            )
            .await;
        match result {
            Ok(result) => {
                let answer = projection_prompt_text(&result.result_projection, "/text");
                if let Some(generation) = generation {
                    let mut cache = self.prompt_runtime.mod_prompt_sections.lock().await;
                    if cache.generation == generation {
                        cache
                            .answers
                            .insert(name_units, (source_units, answer.clone()));
                    }
                }
                answer
            }
            Err(error) => {
                tracing::warn!(section = %name.display_text(), %error, "prompt.section Mod failed");
                text.cloned()
            }
        }
    }

    async fn dispatch_mod_prompt_compose(
        &self,
        host: &std::sync::Arc<hooks::mods::ModHost>,
        ctx: &crate::prompt::SystemPromptContext,
        style: Option<crate::prompt::ActiveOutputStyle<'_>>,
        sections: Vec<crate::prompt::PromptSection>,
    ) -> Vec<crate::prompt::PromptSection> {
        use crate::prompt::PromptSectionScope;
        let input_value =
            mod_prompt_compose_facts(ctx, style, &self.mod_prompt_surfaces(ctx.is_interactive));
        let input = input_value;
        let core_result = mod_prompt_compose_result(&sections);
        let expected = input.clone();
        let log_output = self.output.clone();
        let toast_output = self.output.clone();
        let status_output = self.output.clone();
        let cwd = self.session_cwd.cwd();
        let result = host
            .dispatch_with_utf16_at_context(
                "prompt.compose",
                input,
                &cwd,
                Some(self),
                None,
                None,
                None,
                lingxi_core::host::task_registry::FieldPresence::Missing,
                move |forwarded| {
                    let expected = expected.clone();
                    let core_result = core_result.clone();
                    async move {
                        let Some(forwarded) = forwarded else {
                            return Err(hooks::mods::ModError::Hook(
                                "prompt.compose needs forwarded facts".into(),
                            ));
                        };
                        if forwarded.string_units("/model") != expected.string_units("/model") {
                            return Err(hooks::mods::ModError::Hook(
                                "prompt.compose model is pinned".into(),
                            ));
                        }
                        if forwarded.semantically_eq(&expected) {
                            Ok(Some(core_result))
                        } else {
                            self.mod_prompt_compose_core(forwarded, None, None)
                                .await
                                .map(Some)
                        }
                    }
                },
                move |plugin, text| {
                    let output = log_output.clone();
                    async move { output.emit_mod_log(&plugin, &text).await }
                },
                move |plugin, text, timeout_ms| {
                    let output = toast_output.clone();
                    async move { output.emit_mod_toast(&plugin, &text, timeout_ms).await }
                },
                move |plugin, text| {
                    let output = status_output.clone();
                    async move { output.emit_mod_status(&plugin, text.as_ref()).await }
                },
            )
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(%error, "prompt.compose Mod failed");
                return sections;
            }
        };
        let result = result.result_projection;
        let Some(section_count) = result.array_len("/sections") else {
            return sections;
        };
        if section_count > 512 {
            return sections;
        }
        let mut seen = std::collections::HashSet::<Vec<u16>>::new();
        let mut in_session = false;
        let mut total_bytes = 0usize;
        let mut composed = Vec::with_capacity(section_count);
        for index in 0..section_count {
            let (Some(id), Some(text), Some(scope)) = (
                projection_prompt_text(&result, &format!("/sections/{index}/id")),
                projection_prompt_text(&result, &format!("/sections/{index}/text")),
                projection_string(&result, &format!("/sections/{index}/scope")),
            ) else {
                return sections;
            };
            let id_units = id.utf16_code_units().to_vec();
            if id_units.is_empty() || !seen.insert(id_units.clone()) {
                return sections;
            }
            let scope = match scope.as_str() {
                "shared" if !in_session => PromptSectionScope::Shared,
                "session" => {
                    in_session = true;
                    PromptSectionScope::Session
                }
                _ => return sections,
            };
            total_bytes = total_bytes
                .saturating_add(id_units.len().saturating_mul(2))
                .saturating_add(text.utf16_code_units().len().saturating_mul(2));
            if total_bytes > 4 * 1024 * 1024 {
                return sections;
            }
            composed.push(crate::prompt::PromptSection {
                id: id.display_text().to_owned(),
                text: text.display_text().to_owned(),
                scope,
                id_utf16_code_units: String::from_utf16(&id_units).is_err().then_some(id_units),
                text_utf16_code_units: String::from_utf16(text.utf16_code_units())
                    .is_err()
                    .then(|| text.utf16_code_units().to_vec()),
            });
        }
        composed
    }

    pub(crate) async fn mod_prompt_compose_facts(
        &self,
        overrides: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, hooks::mods::ModError> {
        let override_object = overrides.object_entries().ok_or_else(|| {
            hooks::mods::ModError::Hook("prompt.compose takes the facts to compose for".into())
        })?;
        let ctx = self.build_prompt_context().await.map_err(|error| {
            hooks::mods::ModError::Protocol(
                error
                    .diagnostic_projection()
                    .to_json_string()
                    .unwrap_or_else(|_| "settings operation failed".to_owned()),
            )
        })?;
        let resolved = self.resolve_active_output_style().await;
        let style = resolved
            .as_ref()
            .map(|style| crate::prompt::ActiveOutputStyle {
                name: &style.name,
                prompt: &style.prompt,
                keep_coding_instructions: style.keep_coding_instructions,
            });
        let mut facts =
            mod_prompt_compose_facts(&ctx, style, &self.mod_prompt_surfaces(ctx.is_interactive));
        const ALLOWED: &[&str] = &[
            "model",
            "promptModel",
            "surfaces",
            "tools",
            "outputStyle",
            "traits",
        ];
        for entry in override_object {
            let Some(key) = ALLOWED
                .iter()
                .copied()
                .find(|key| entry.code_units.iter().copied().eq(key.encode_utf16()))
            else {
                return Err(hooks::mods::ModError::Hook(format!(
                    "prompt.compose has unknown fact {}",
                    entry.display_key
                )));
            };
            let value = overrides
                .subprojection(&format!("/{key}"))
                .map_err(|error| hooks::mods::ModError::Protocol(error.to_string()))?;
            facts
                .set_field(key, value)
                .map_err(|error| hooks::mods::ModError::Protocol(error.to_string()))?;
        }
        validate_mod_prompt_compose_facts(&facts)?;
        Ok(facts)
    }

    pub(crate) async fn mod_prompt_compose_core(
        &self,
        facts: Utf16JsonProjection,
        origin: Option<serde_json::Value>,
        skip_hook_id: Option<u64>,
    ) -> Result<Utf16JsonProjection, hooks::mods::ModError> {
        validate_mod_prompt_compose_facts(&facts)?;
        let model = projection_string(&facts, "/model")
            .ok_or_else(|| hooks::mods::ModError::Hook("prompt.compose needs a model".into()))?;
        let requested_tools = facts.array_len("/tools").unwrap_or_default();
        let traits = facts.array_len("/traits").unwrap_or_default();
        let mut ctx = self.build_prompt_context().await.map_err(|error| {
            hooks::mods::ModError::Protocol(
                error
                    .diagnostic_projection()
                    .to_json_string()
                    .unwrap_or_else(|_| "settings operation failed".to_owned()),
            )
        })?;
        if ctx.model != model {
            ctx.model = model.clone();
            ctx.model_marketing_name = crate::prompt::env_meta::marketing_name_for_model(&model)
                .map(String::from)
                .or_else(|| {
                    (!model.to_ascii_lowercase().contains("claude"))
                        .then(|| crate::provider_adapter::display_name_for_model(&model))
                        .flatten()
                });
            ctx.knowledge_cutoff =
                crate::prompt::env_meta::knowledge_cutoff_for_model(&model).map(String::from);
        }
        let available = ctx
            .tool_names
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let mut seen = std::collections::HashSet::<Vec<u16>>::new();
        ctx.tool_names = (0..requested_tools)
            .filter_map(|index| {
                let units = facts.string_units(&format!("/tools/{index}"))?;
                let name = String::from_utf16(&units).ok()?;
                (available.contains(&name) && seen.insert(units)).then_some(name)
            })
            .collect();
        ctx.exclude_dynamic_sections = (0..traits).any(|index| {
            facts
                .string_units(&format!("/traits/{index}"))
                .is_some_and(|units| units.iter().copied().eq("sdk-preset".encode_utf16()))
        });
        let resolved = self.resolve_active_output_style().await;
        let style = resolved
            .as_ref()
            .map(|style| crate::prompt::ActiveOutputStyle {
                name: &style.name,
                prompt: &style.prompt,
                keep_coding_instructions: style.keep_coding_instructions,
            });
        let sections = crate::prompt::named_prompt_sections_with_style(&ctx, style);
        let host = if let Some(registry) = &self.lifecycle_runtime.hook_registry {
            registry.read().await.mod_host()
        } else {
            None
        };
        let composed = if let Some(host) = host.as_ref() {
            futures::future::join_all(
                crate::prompt::prompt_section_slots(sections, &ctx)
                    .into_iter()
                    .map(|slot| {
                        let origin = origin.clone();
                        async move {
                            let id = slot.id_prompt();
                            let slot_text = slot.text_prompt();
                            let text = if slot.scope == crate::prompt::PromptSectionScope::Session
                                && !id.equals_ascii("output_style")
                            {
                                self.dispatch_mod_prompt_section(
                                    Some(host),
                                    &id,
                                    slot_text.as_ref(),
                                    false,
                                    origin,
                                    skip_hook_id,
                                )
                                .await
                            } else {
                                slot_text
                            };
                            text.map(|text| {
                                let text_utf16_code_units =
                                    String::from_utf16(text.utf16_code_units())
                                        .is_err()
                                        .then(|| text.utf16_code_units().to_vec());
                                crate::prompt::PromptSection {
                                    id: slot.id,
                                    text: text.display_text().to_owned(),
                                    scope: slot.scope,
                                    id_utf16_code_units: slot.id_utf16_code_units,
                                    text_utf16_code_units,
                                }
                            })
                        }
                    }),
            )
            .await
            .into_iter()
            .flatten()
            .collect()
        } else {
            sections
        };
        Ok(mod_prompt_compose_result(&composed))
    }

    /// Return the current dynamic `systemContext` contribution. Today the
    /// port models Claude's `gitStatus` member; the exclusion flag omits the
    /// whole context just as the upstream custom/static path does.
    async fn current_system_context_block(&self) -> Option<String> {
        if self.config.exclude_dynamic_system_prompt_sections || self.uses_announced_context().await
        {
            return None;
        }
        let cwd = self.session_cwd.cwd();
        let probe_cwd = self.prompt_probe_cwd(&cwd);
        self.cached_git_status(&probe_cwd).await.1
    }

    async fn append_dynamic_system_context(&self, prompt: &mut String) -> Option<String> {
        let block = self.current_system_context_block().await;
        if let Some(block) = &block {
            prompt.push_str("\n\n");
            prompt.push_str(block);
        }
        block
    }

    /// Load the host context and render its outgoing-only inline projection.
    /// Normal Or routing consumes the captured typed files and context fields
    /// through `context_announcement_messages` instead of sending this prefix.
    ///
    /// 1:1 with claude-code `A6n(messages, userContext)` (binary offset
    /// ~205838418): when the `userContext` object is non-empty it PREPENDS one
    /// `isMeta` user message whose body is
    /// ```text
    /// <system-reminder>
    /// As you answer the user's questions, you can use the following context:
    /// # {key}
    /// {value}
    /// …                          (one `# {key}\n{value}` per entry, joined by `\n`)
    ///
    ///       IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.
    /// </system-reminder>
    /// ```
    /// (the IMPORTANT line is indented by exactly six spaces).
    ///
    /// The `userContext` keys, in claude-code insertion order (`pS`,
    /// binary offset ~197202100): `instructions` (the assembled LINGXI.md memory
    /// block — [`memory_block::format`]), `userEmail`
    /// (`The user's email address is {email}.`, only when configured), and
    /// `currentDate` (`Today's date is {YYYY-MM-DD}.`, always present). The
    /// `attachedProject` key (CLAUDE_PROJECT_TOOL) is not modelled.
    ///
    /// NOTE: `gitStatus` is NOT here — it belongs to the SEPARATE `systemContext`
    /// that claude-code folds into the SYSTEM PROMPT (see `build_system_prompt`),
    /// not this `userContext` message.
    ///
    /// Inline projection of the memoized Gv blocks plus the dynamic Environment.
    /// The scalar projection is outgoing-only; announced routing persists typed
    /// instruction/context attachments through the ordinary producer.
    async fn dispatch_mod_prompt_context(
        &self,
        initial: Utf16JsonProjection,
    ) -> Utf16JsonProjection {
        let Some(registry) = &self.lifecycle_runtime.hook_registry else {
            return initial;
        };
        let Some(host) = registry.read().await.mod_host() else {
            return initial;
        };
        let log_output = self.output.clone();
        let toast_output = self.output.clone();
        let status_output = self.output.clone();
        let cwd = self.session_cwd.cwd();
        let result = host
            .dispatch_with_utf16_at_context(
                "prompt.context",
                initial.clone(),
                &cwd,
                Some(self),
                None,
                None,
                None,
                lingxi_core::host::task_registry::FieldPresence::Missing,
                |event| async move { Ok(event) },
                move |plugin, text| {
                    let output = log_output.clone();
                    async move { output.emit_mod_log(&plugin, &text).await }
                },
                move |plugin, text, timeout_ms| {
                    let output = toast_output.clone();
                    async move { output.emit_mod_toast(&plugin, &text, timeout_ms).await }
                },
                move |plugin, text| {
                    let output = status_output.clone();
                    async move { output.emit_mod_status(&plugin, text.as_ref()).await }
                },
            )
            .await;
        match result {
            Ok(result) => result.result_projection,
            Err(error) => {
                tracing::warn!(%error, "prompt.context Mod failed");
                initial
            }
        }
    }

    async fn resolved_mod_instruction_context(
        &self,
        key: lingxi_core::host::instructions::InstructionContextKey,
        load: &lingxi_core::host::instruction_context_cache::InstructionContextLoad<
            crate::prompt::MemoryFile,
        >,
        cached: &lingxi_core::host::instructions::InstructionContext,
    ) -> Utf16JsonProjection {
        let mod_identity = if let Some(registry) = &self.lifecycle_runtime.hook_registry {
            registry
                .read()
                .await
                .mod_host()
                .map(|host| host.registration_identity())
        } else {
            None
        };
        // The hook resolves the frozen Gv blocks. A cwd-only eager-file reset
        // does not acquire a new context or freeze the query's Environment.
        let context = (
            key.session_id.to_string(),
            cached
                .instructions_root
                .clone()
                .unwrap_or_else(|| self.cwd.clone()),
            mod_identity,
        );
        let generation = {
            let mut cache = self.prompt_runtime.mod_prompt_context.lock().await;
            if cache.context.as_ref() != Some(&context)
                || !cache
                    .instruction_load
                    .as_ref()
                    .is_some_and(|prior| prior.same_build(load))
            {
                cache.generation = cache.generation.wrapping_add(1);
                cache.context = Some(context.clone());
                cache.instruction_load = Some(load.clone());
                cache.resolved = None;
            }
            if let Some(resolved) = &cache.resolved {
                return resolved.clone();
            }
            cache.generation
        };
        let memory_files = load.memory_files().await;
        let blocks = cached
            .user_context_entries()
            .into_iter()
            .map(|(name, text)| {
                let name = exact_string_projection(&String::from_utf16_lossy(&name), &name);
                projection_object(vec![("name".into(), name), ("text".into(), text)])
            })
            .collect::<Vec<_>>();
        let native_managed = self.memory.native_managed_instruction_files();
        let instruction_files =
            projection_array(mod_instruction_files(&memory_files, &native_managed));
        let initial = projection_object(vec![
            ("blocks".into(), projection_array(blocks)),
            ("instructionFiles".into(), instruction_files),
        ]);
        let mut resolved = self.dispatch_mod_prompt_context(initial).await;
        // Native W1n freezes Object.fromEntries: repeated names replace their
        // value while retaining the first insertion position.
        if let Some(block_count) = resolved.array_len("/blocks") {
            let source = resolved.clone();
            let mut positions = std::collections::HashMap::<Vec<u16>, usize>::new();
            let mut normalized = Vec::new();
            for source_index in 0..block_count {
                let (Some(name_units), Some(text_units)) = (
                    source.string_units(&format!("/blocks/{source_index}/name")),
                    source.string_units(&format!("/blocks/{source_index}/text")),
                ) else {
                    continue;
                };
                let name =
                    exact_string_projection(&String::from_utf16_lossy(&name_units), &name_units);
                let text =
                    exact_string_projection(&String::from_utf16_lossy(&text_units), &text_units);
                let block = projection_object(vec![("name".into(), name), ("text".into(), text)]);
                let output_index = if let Some(index) = positions.get(&name_units).copied() {
                    normalized[index] = block;
                    index
                } else {
                    let index = normalized.len();
                    positions.insert(name_units, index);
                    normalized.push(block);
                    index
                };
                let _ = output_index;
            }
            resolved
                .set_field("blocks", projection_array(normalized))
                .expect("normalized prompt blocks form a valid projection");
        }
        let mut cache = self.prompt_runtime.mod_prompt_context.lock().await;
        if cache.generation == generation
            && cache.context.as_ref() == Some(&context)
            && cache
                .instruction_load
                .as_ref()
                .is_some_and(|prior| prior.same_build(load))
        {
            cache.resolved = Some(resolved.clone());
        }
        resolved
    }

    pub(crate) fn source_settings_failure(
        kind: &str,
        error: String,
    ) -> lingxi_core::settings::SettingsError {
        use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
        let message = Source::from(error.as_str());
        let diagnostics = Source::object(vec![
            ("kind".into(), Source::from(kind)),
            ("message".into(), message.clone()),
        ])
        .expect("source failure diagnostic is a valid object");
        lingxi_core::settings::SettingsError::source_failure(kind, message, diagnostics, None)
    }

    pub(crate) async fn additional_context_message(
        &self,
    ) -> Result<Option<ConversationMessage>, lingxi_core::settings::SettingsError> {
        let (key, load) = self.main_instruction_load().await;
        self.additional_context_message_from_load(key, &load).await
    }

    /// Query preparation retains its original Gv handle across host refreshes.
    /// Acquisition failure propagates before any model request is dispatched.
    pub(crate) async fn additional_context_message_from_load(
        &self,
        key: lingxi_core::host::instructions::InstructionContextKey,
        load: &lingxi_core::host::instruction_context_cache::InstructionContextLoad<
            crate::prompt::MemoryFile,
        >,
    ) -> Result<Option<ConversationMessage>, lingxi_core::settings::SettingsError> {
        if self.config.bare {
            return Ok(None);
        }
        // `instructions` value = the assembled memory block (preamble + `Contents
        // of …:` blocks). Empty when no LINGXI.md files are loaded.
        //
        // Task 5 (worktree 206 session-cwd plumbing): read the LIVE
        // `self.session_cwd.cwd()`, not the frozen `self.cwd` — this reminder
        // uses the current root cache. Worktree changes invalidate qb explicitly;
        // Gv retains its snapshot until its distinct native refresh event.
        //
        // PathAtlas S3: probe the HOST directory backing the (possibly guest)
        // session cwd — same hop `build_prompt_context` makes. This message is
        // the ONLY render path for the memory block (`prompt/mod.rs` no longer
        // splices it into the system prompt), so reading the raw guest path
        // here meant a mobile workspace `LINGXI.md` reached the model NOWHERE.
        // Build the entries in claude-code insertion order; each is `# key\nvalue`.
        let mut blocks = Vec::with_capacity(4);
        // `--exclude-dynamic-system-prompt-sections`: the per-machine env block
        // (cwd / env / git / OS / shell) is OMITTED from the static system prompt
        // (see `assemble_system_prompt_with_style`) and re-emitted HERE in the
        // first-user-message context reminder, so the system prompt stays
        // identical across machines (prompt-cache reuse) while the model still
        // sees the env. Built from the SAME `build_prompt_context` the system
        // prompt uses. (`false` ⟶ skipped, byte-identical to before this flag.)
        //
        // claude-code: "Only applies with the default system prompt (ignored with
        // --system-prompt)." A custom system prompt already bypasses the static
        // env block, so re-emitting it here under `--system-prompt` would leak
        // per-machine env into the first user message that the oracle never sends.
        // Gate the env-block re-emission on the absence of a system-prompt override
        // so the exclude-dynamic flag is a complete no-op when a custom prompt is
        // active. (The instructions / userEmail / currentDate entries below stay
        // unconditional — they are unrelated to this flag.)
        //
        // Gv and every projection retain one original asynchronous build.
        // Dynamic Environment remains the ordinary query-level producer.
        let cached = load
            .get()
            .await
            .map_err(|error| Self::source_settings_failure("instruction_context", error))?;
        self.capture_cached_instruction_context(key, &cached).await;
        let resolved = self
            .resolved_mod_instruction_context(key, load, &cached)
            .await;
        let exclude_env = self.config.exclude_dynamic_system_prompt_sections
            && self.config.system_prompt_override.is_none();
        if exclude_env {
            let files = load.memory_files().await;
            let ctx = self
                .build_prompt_context_from_files(files.as_ref().clone())
                .await?;
            let env = crate::prompt::env_block::format(&ctx);
            let body = env.strip_prefix("# Environment\n").unwrap_or(&env).trim();
            if !body.is_empty() {
                blocks.push(projection_object(vec![
                    ("name".into(), "Environment".into()),
                    ("text".into(), body.into()),
                ]));
            }
        }
        for source_index in 0..resolved.array_len("/blocks").unwrap_or_default() {
            let (Some(name_units), Some(text_units)) = (
                resolved.string_units(&format!("/blocks/{source_index}/name")),
                resolved.string_units(&format!("/blocks/{source_index}/text")),
            ) else {
                continue;
            };
            if !text_units.is_empty() {
                blocks.push(projection_object(vec![
                    (
                        "name".into(),
                        exact_string_projection(
                            &String::from_utf16_lossy(&name_units),
                            &name_units,
                        ),
                    ),
                    (
                        "text".into(),
                        exact_string_projection(
                            &String::from_utf16_lossy(&text_units),
                            &text_units,
                        ),
                    ),
                ]));
            }
        }
        // Forks inherit the exact parent user-context envelope, including its
        // non-instruction keys. omitClaudeMd replaces only claudeMd later.
        let session = self.session.lock().await;
        let mut stored = self.prompt_runtime.instruction_context.lock().await;
        let accepted = session.session_id == key.session_id
            && self
                .prompt_runtime
                .instruction_cache
                .is_current_context(key, &cached)
            && self
                .prompt_runtime
                .instruction_context_origin
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .is_some_and(|origin| Arc::ptr_eq(origin, &cached));
        if let Some(context) = stored.as_mut().filter(|_| accepted) {
            context.user_context = projection_object(Vec::new());
            for index in 0..resolved.array_len("/blocks").unwrap_or_default() {
                if let (Some(name), Some(text)) = (
                    resolved.string_units(&format!("/blocks/{index}/name")),
                    resolved.string_units(&format!("/blocks/{index}/text")),
                ) {
                    let value = exact_string_projection(&String::from_utf16_lossy(&text), &text);
                    context
                        .set_user_context_value(&name, value)
                        .expect("resolved Mod userContext remains an ordered object");
                }
            }
            context.eager_instructions = resolved
                .at_pointer("/instructionFiles")
                .and_then(|files| files.array_len())
                .map(|count| {
                    use lingxi_core::host::instructions::{InstructionFile, InstructionFileType};
                    (0..count)
                        .filter_map(|index| {
                            let pointer = format!("/instructionFiles/{index}");
                            let file = resolved.at_pointer(&pointer)?;
                            let path = resolved.subprojection(&format!("{pointer}/path")).ok()?;
                            let kind = match file.get_property("kind")?.as_str()? {
                                "managed" => InstructionFileType::Managed,
                                "user" => InstructionFileType::User,
                                "project" => InstructionFileType::Project,
                                "local" => InstructionFileType::Local,
                                "memory" => InstructionFileType::AutoMem,
                                _ => return None,
                            };
                            let content =
                                resolved.subprojection(&format!("{pointer}/content")).ok()?;
                            Some(InstructionFile::new(
                                path,
                                kind,
                                exact_string_projection(
                                    &String::from_utf16_lossy(
                                        &crate::prompt::memory_block::trim_instruction_content(
                                            &content.string_units("")?,
                                        ),
                                    ),
                                    &crate::prompt::memory_block::trim_instruction_content(
                                        &content.string_units("")?,
                                    ),
                                ),
                            ))
                        })
                        .collect()
                });
        }

        drop(stored);
        drop(session);

        // `A6n` returns the messages unchanged when the context object is empty.
        // `currentDate` is always present, so `entries` is never empty — but keep
        // the guard for faithfulness to the `Object.entries(t).length===0` check.
        if blocks.is_empty() {
            return Ok(None);
        }
        Ok(render_mod_prompt_context(&projection_object(vec![(
            "blocks".into(),
            projection_array(blocks),
        )])))
    }

    /// Resolve this session's plan file path (206 `ON(agentId)` →
    /// `<plansDir>/<slug>.md`). The plans directory is resolved by
    /// [`Self::plans_dir`] (206 `iT`): the `plansDirectory` settings override
    /// (relative to the project root, with a within-root containment check) when
    /// present, else the default `<project-root>/.lingxi/plans/`. The slug is the
    /// session's UUID (206's slug is likewise session-specific — exact bytes are
    /// not observable, the structure is). Uses the bare UUID (not the `sess:`
    /// display form) so the filename has no `:` separator, matching
    /// `computed_transcript_path`.
    /// The session's plan file, preferring the shared identity when the host
    /// published one (2.1.266 `ay(void 0)`); otherwise the pre-slug derivation.
    pub(crate) fn session_plan_file_path(&self, session_id: &SessionId) -> String {
        if let Some(plan_files) = self.config.plan_files.as_ref() {
            if let Some(path) = plan_files.plan_file(None) {
                return path.to_string_lossy().into_owned();
            }
        }
        Self::plan_file_path(
            session_id,
            &self.cwd,
            self.config.plans_directory.as_deref(),
        )
    }

    pub fn plan_file_path(
        session_id: &SessionId,
        project_root: &std::path::Path,
        plans_directory: Option<&str>,
    ) -> String {
        Self::plans_dir(project_root, plans_directory)
            .join(format!("{}.md", session_id.as_uuid()))
            .to_string_lossy()
            .into_owned()
    }

    /// Resolve the plans DIRECTORY — 1:1 with the binary's `iT`:
    ///
    /// ```js
    /// iT=Or(function(){
    ///   let r=Wn().plansDirectory;
    ///   if(r){
    ///     let n=Ct(),o=Pne.resolve(n,r);
    ///     if(W5_(o,n))return o;
    ///     C(`plansDirectory must be within project root: ${r}`,{level:"error"})
    ///   }
    ///   return KPp()  // Pne.join(mn(),"plans")
    /// })
    /// ```
    ///
    /// When `plansDirectory` is set: resolve it against the project root
    /// (absolute values are used verbatim, `path.resolve` semantics), normalize
    /// `.`/`..` lexically, and accept it only if it is WITHIN the project root
    /// (`W5_`'s primary check `o === n || o.startsWith(n + sep)`) and passes
    /// the hardened protected-dir / same-repo-root checks. On rejection, fall
    /// through to [`Self::default_plans_dir`].
    ///
    /// DIVERGENCE (default location): upstream's `KPp()` is
    /// `<config-home>/plans/` (`~/.claude/plans/`). LingXi's file tools only
    /// write inside their trusted set (the project cwd plus additional working
    /// dirs), so a plan file under the config home is rejected with
    /// `ToolError::PathBlocked`, the model falls back to pasting the plan into
    /// the chat, and `ExitPlanMode` reads back an EMPTY plan. The default is
    /// therefore `<project-root>/.lingxi/plans/`, inside the trusted set.
    pub fn plans_dir(
        project_root: &std::path::Path,
        plans_directory: Option<&str>,
    ) -> std::path::PathBuf {
        if let Some(r) = plans_directory.filter(|s| !s.is_empty()) {
            // `path.resolve(project_root, r)`: absolute `r` wins; else join.
            let candidate = if std::path::Path::new(r).is_absolute() {
                std::path::PathBuf::from(r)
            } else {
                project_root.join(r)
            };
            let resolved = crate::turn_loop::normalize_lexically(&candidate);
            let root = crate::turn_loop::normalize_lexically(project_root);
            // `W5_` primary: `o === n || o.startsWith(n + sep)` — component-wise
            // prefix containment on the normalized paths (so a `../escape` that
            // popped above `root` is rejected).
            if confined_path_components(&root, &resolved).is_some() {
                if plans_dir_passes_hardening(project_root, &root, &resolved) {
                    return resolved;
                }
                tracing::warn!("plansDirectory rejected by hardening guard: {r}");
                return Self::default_plans_dir(project_root);
            }
            tracing::error!("plansDirectory must be within project root: {r}");
        }
        Self::default_plans_dir(project_root)
    }

    /// The default plans directory — `<project-root>/.lingxi/plans/`.
    ///
    /// Project-local on purpose: the file tools' trusted set is the project cwd
    /// plus additional working dirs, so this is the one location the plan-mode
    /// reminder can name and the model can actually write. Repos that do not
    /// want plans in `git status` ignore `.lingxi/`.
    pub fn default_plans_dir(project_root: &std::path::Path) -> std::path::PathBuf {
        project_root.join(branding::DOT_DIR).join("plans")
    }

    async fn mobile_runtime_environment_message(&self) -> Option<ConversationMessage> {
        if let Some(message) = &self.mobile_runtime_environment_message {
            return Some(message.clone());
        }
        let environment = self.mobile_runtime_environment.as_ref()?;
        Some(ConversationMessage::user_meta(
            MessageId::new(),
            environment.render_system_reminder(),
        ))
    }
}

// Plans-directory confinement helpers live with prompt/plan path assembly.
//
// `.lingxi` is deliberately ABSENT: it is where the default plans directory
// lives (`<project-root>/.lingxi/plans`), so listing it would reject an
// explicit `plansDirectory: ".lingxi/plans"` and fall back to the byte-identical
// default with a spurious "rejected by hardening guard" warning.
pub(super) const PROTECTED_PLANS_DIR_COMPONENTS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".bzr",
    ".jj",
    ".sl",
    ".claude",
    ".cargo",
    "node_modules",
];

pub(super) fn normalize_guard_component(component: &std::ffi::OsStr) -> Option<String> {
    let text = component
        .to_str()?
        .trim_end_matches(['.', ' '])
        .to_ascii_lowercase();
    (!text.is_empty()).then_some(text)
}

pub(super) fn path_relative_components(
    root: &std::path::Path,
    candidate: &std::path::Path,
    case_insensitive: bool,
) -> Option<Vec<std::ffi::OsString>> {
    let mut candidate_components = candidate.components();
    for root_component in root.components() {
        let candidate_component = candidate_components.next()?;
        let equal = if case_insensitive {
            root_component
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&candidate_component.as_os_str().to_string_lossy())
        } else {
            root_component == candidate_component
        };
        if !equal {
            return None;
        }
    }
    Some(
        candidate_components
            .map(|component| component.as_os_str().to_os_string())
            .collect(),
    )
}

pub(super) fn confined_path_components(
    root: &std::path::Path,
    candidate: &std::path::Path,
) -> Option<Vec<std::ffi::OsString>> {
    path_relative_components(root, candidate, cfg!(windows))
}

pub(super) fn plans_dir_has_protected_component(
    root: &std::path::Path,
    candidate: &std::path::Path,
) -> bool {
    confined_path_components(root, candidate).is_none_or(|components| {
        components.iter().any(|name| {
            normalize_guard_component(name).is_some_and(|name| {
                PROTECTED_PLANS_DIR_COMPONENTS
                    .iter()
                    .any(|protected| name == *protected)
            })
        })
    })
}

pub(super) fn deepest_existing_ancestor(path: &std::path::Path) -> Option<&std::path::Path> {
    path.ancestors().find(|ancestor| ancestor.exists())
}

pub(super) fn metadata_is_link_like(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    false
}

pub(super) fn has_symlink_component_between(
    root: &std::path::Path,
    existing: &std::path::Path,
) -> Option<bool> {
    let mut current = root.to_path_buf();
    for component in confined_path_components(root, existing)? {
        current.push(component);
        let meta = std::fs::symlink_metadata(&current).ok()?;
        if metadata_is_link_like(&meta) {
            return Some(true);
        }
    }
    Some(false)
}

pub(super) fn nearest_repo_root(path: &std::path::Path) -> Option<std::path::PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .and_then(|ancestor| std::fs::canonicalize(ancestor).ok())
}

pub(super) fn plans_dir_passes_hardening(
    project_root: &std::path::Path,
    normalized_root: &std::path::Path,
    normalized_candidate: &std::path::Path,
) -> bool {
    if plans_dir_has_protected_component(normalized_root, normalized_candidate) {
        return false;
    }

    let canonical_root = match std::fs::canonicalize(project_root) {
        Ok(path) => path,
        Err(_) => return false,
    };
    let existing = match deepest_existing_ancestor(normalized_candidate) {
        Some(path) => path,
        None => return false,
    };
    if !existing.is_dir() {
        return false;
    }
    if has_symlink_component_between(normalized_root, existing) != Some(false) {
        return false;
    }

    let canonical_existing = match std::fs::canonicalize(existing) {
        Ok(path) => path,
        Err(_) => return false,
    };
    if confined_path_components(&canonical_root, &canonical_existing).is_none() {
        return false;
    }

    let project_repo_root = nearest_repo_root(&canonical_root);
    let candidate_repo_root = nearest_repo_root(&canonical_existing);
    match (project_repo_root, candidate_repo_root) {
        (Some(project), Some(candidate)) => project == candidate,
        (None, None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod typed_attachment_snapshot_tests {
    use super::*;

    #[test]
    fn appended_native_system_prompt_keeps_projection_in_snapshot_and_sections() {
        let units = ["policy ".encode_utf16().collect::<Vec<_>>(), vec![0xd800]].concat();
        let source =
            Utf16JsonProjection::root_string("policy �".to_owned(), units.clone()).unwrap();
        let mut prompt = crate::prompt::RenderedPromptSections {
            text: "base".to_owned(),
            static_text: "base".to_owned(),
            source_sections: Vec::new(),
            snapshot_system_prompt: Some(vec![
                lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string(
                    "base",
                ),
            ]),
            dynamic_system_context: None,
        };

        append_source_system_prompt(&mut prompt, &source);

        assert_eq!(prompt.text, "base\n\npolicy �");
        assert_eq!(
            prompt
                .snapshot_system_prompt
                .as_ref()
                .unwrap()
                .last()
                .unwrap()
                .utf16_code_units(),
            units.as_slice()
        );

        let mut prompt = crate::prompt::RenderedPromptSections {
            text: "base".to_owned(),
            static_text: "base".to_owned(),
            source_sections: vec![crate::prompt::PromptSection {
                id: "base".to_owned(),
                text: "base".to_owned(),
                scope: crate::prompt::PromptSectionScope::Shared,
                id_utf16_code_units: None,
                text_utf16_code_units: None,
            }],
            snapshot_system_prompt: None,
            dynamic_system_context: None,
        };
        append_source_system_prompt(&mut prompt, &source);
        assert_eq!(
            prompt
                .source_sections
                .last()
                .unwrap()
                .text_utf16_code_units
                .as_deref(),
            Some(units.as_slice())
        );
    }

    #[test]
    fn explicit_typed_identity_includes_meta_user_and_excludes_other_rows() {
        let typed_meta = ConversationMessage::user_meta(
            lingxi_core::types::MessageId::new(),
            "typed attachment".into(),
        );
        let generic_meta = ConversationMessage::user_meta(
            lingxi_core::types::MessageId::new(),
            "generic meta".into(),
        );
        let ordinary_user =
            ConversationMessage::user(lingxi_core::types::MessageId::new(), "ordinary user".into());
        let typed_id = typed_meta.id();
        let messages = vec![typed_meta, generic_meta, ordinary_user];
        let typed_ids = std::collections::HashSet::from([typed_id]);

        assert_eq!(
            typed_attachment_ids_in_snapshot(&typed_ids, &messages),
            typed_ids
        );
    }

    #[tokio::test]
    async fn live_registered_hook_attachment_reorders_before_skill_without_rewriting_history() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };

        let orch = ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            std::sync::Arc::new(MockApiClient::new(Vec::new())),
            std::sync::Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            std::sync::Arc::new(NoOpPermissionGate),
            std::sync::Arc::new(MockOutputStream::new()),
            std::sync::Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        let header = ConversationMessage::user(MessageId::new(), "command header".into());
        let body = ConversationMessage::user_meta(MessageId::new(), "skill body".into());
        let generic = ConversationMessage::user_meta(MessageId::new(), "ordinary meta".into());
        let payload = Utf16JsonProjection::parse(
            r#"{"type":"hook_additional_context","hookEvent":"UserPromptExpansion","content":["UPE \ud800"]}"#,
        )
        .unwrap();
        let hook = ConversationOrchestrator::native_hook_additional_context_message_for_api(
            MessageId::new(),
            &payload,
        )
        .unwrap();
        let hook_id = hook.id();
        let expected_units = match &hook {
            ConversationMessage::User { content, .. } => match &content[0] {
                lingxi_core::types::ContentBlock::TextJsUtf16 {
                    utf16_code_units, ..
                } => utf16_code_units.clone(),
                other => panic!("source-preserving hook text expected: {other:?}"),
            },
            other => panic!("user hook rendering expected: {other:?}"),
        };
        let history = vec![header.clone(), body.clone(), hook.clone(), generic.clone()];
        orch.session.lock().await.history = history.clone();
        orch.register_mod_persisted_attachment(
            &hook,
            "hook_additional_context",
            serde_json::json!({"kind":"hook","event":"UserPromptExpansion"}),
        )
        .await;
        assert!(orch
            .transcript
            .model_reminder_attachments
            .lock()
            .unwrap()
            .is_empty());

        let mut request = history.clone();
        orch.project_typed_attachment_order(&mut request, &[], &[], false)
            .await;
        assert_eq!(
            request
                .iter()
                .map(ConversationMessage::id)
                .collect::<Vec<_>>(),
            vec![hook_id, header.id(), body.id(), generic.id()]
        );
        assert_eq!(orch.session.lock().await.history, history);
        assert_eq!(
            orch.typed_attachment_ids_for_snapshot(&request),
            std::collections::HashSet::from([hook_id])
        );
        let ConversationMessage::User { content, .. } = &request[0] else {
            panic!("hook attachment remains a user message");
        };
        let lingxi_core::types::ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } = &content[0]
        else {
            panic!("hook source units remain exact");
        };
        assert_eq!(utf16_code_units, &expected_units);

        let first_projection = request.clone();
        orch.project_typed_attachment_order(&mut request, &[], &[], false)
            .await;
        assert_eq!(
            request, first_projection,
            "retry retains attachment identity and order"
        );
    }
}

// ── Task 7 helpers ──────────────────────────────────────────────────────────
