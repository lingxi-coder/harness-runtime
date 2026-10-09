//! Tool-schema assembly, host tool calls, and read-state integration.

use super::*;

impl ConversationOrchestrator {
    /// Seed the live read-state cache from an SDK host's `seed_read_state`
    /// control request.
    ///
    /// Claude Code accepts the host snapshot only when the file is at most
    /// 10 MiB and its floor-truncated on-disk mtime is no newer than the
    /// supplied mtime. Failures are intentionally swallowed by the control
    /// protocol; the boolean is exposed solely so callers and tests can observe
    /// whether an entry was installed.
    pub async fn seed_read_state_from_host(&self, path: &str, host_mtime_ms: f64) -> bool {
        const MAX_SEED_BYTES: u64 = 10 * 1024 * 1024;

        let requested = std::path::PathBuf::from(path);
        let absolute = if requested.is_absolute() {
            requested
        } else {
            self.session_cwd.cwd().join(requested)
        };
        let absolute = crate::turn_loop::normalize_lexically(&absolute);

        let Ok(metadata) = tokio::fs::metadata(&absolute).await else {
            return false;
        };
        if metadata.len() > MAX_SEED_BYTES {
            return false;
        }
        let Ok(modified) = metadata.modified() else {
            return false;
        };
        let Ok(since_epoch) = modified.duration_since(std::time::UNIX_EPOCH) else {
            return false;
        };
        let mtime_ms = since_epoch.as_millis().min(i64::MAX as u128) as i64;
        if (mtime_ms as f64) > host_mtime_ms.floor() {
            return false;
        }
        let Ok(content) = tokio::fs::read_to_string(&absolute).await else {
            return false;
        };
        let content = content
            .strip_prefix('\u{feff}')
            .unwrap_or(&content)
            .replace("\r\n", "\n")
            .replace('\r', "\n");

        tool_api::read_file_state::set_with_model_context(
            &self.prompt_runtime.read_state_map,
            absolute,
            tool_api::read_file_state::ReadFileEntry {
                content,
                mtime_ms,
                offset: None,
                limit: None,
                // The host explicitly marks this content as not present in the
                // model context. `from_read = false` preserves that invariant:
                // the next Read must not deduplicate against this seed.
                from_read: false,
                seeded_from_context: false,
                is_partial_view: false,
            },
            false,
        );
        true
    }

    /// Seed the loaded memory files (LINGXI.md + `.lingxi/rules/**`) into the
    /// shared read-state registry — the port of claude-code's startup loop
    /// `xCt` (2.1.220 @245883373):
    ///
    /// ```text
    /// for(let Fr of yt){
    ///   if(tHt(Fr.path))continue;
    ///   let jn=MLu(Fr),Ao;
    ///   try{Ao=jn?FQ(Fr.path):Date.now()}catch{Ao=Date.now()}
    ///   vM.current.set(Fr.path,{
    ///     content: Fr.contentDiffersFromDisk?Fr.rawContent??Fr.content:X9(Fr.content),
    ///     timestamp: Ao, offset:void 0, limit:void 0,
    ///     isPartialView: Fr.contentDiffersFromDisk,
    ///     seededFromContext: jn,
    ///     ...!jn&&{contentNotInModelContext:!0},
    ///     keepContent:!0}), …}
    /// ```
    ///
    /// This is what makes the Read tool's seeded dedup branch
    /// (`tool_file::read`, @235741459) reachable: the model already received
    /// these files' bodies in the system prompt's memory block, so re-`Read`ing
    /// one returns `FILE_UNCHANGED_SEEDED_PREFIX` instead of a second copy.
    ///
    /// Per-file decisions, each matching the oracle:
    /// - **Skip when already present.** `!readFileState.has(path)` (the site-2
    ///   guard @237715046). Uses the NON-promoting
    ///   [`tool_api::read_file_state::ReadFileStateLru::contains`] so seeding
    ///   never reshuffles LRU order, and so a file the model genuinely `Read`
    ///   is never overwritten by a seed. This is also what makes the
    ///   `reason = Compact` re-entry into
    ///   [`Self::fire_instructions_loaded_with_reason`] idempotent.
    /// - **`seeded_from_context = MLu(f)`** =
    ///   [`crate::prompt::memory_block::is_rendered_into_context`], derived from
    ///   the renderer so the two cannot drift. A `paths:`-gated conditional rule
    ///   is NOT rendered, so it seeds with `false` — its content is NOT in
    ///   context and must never dedup.
    /// - **`mtime_ms`** = the on-disk mtime for a rendered file (`FQ(path)`),
    ///   `Date.now()` otherwise. ANY stat error falls back to `now` — the
    ///   oracle wraps the whole thing in `try{…}catch{Ao=Date.now()}`, so this
    ///   never propagates and never panics.
    /// - **`content`**. LingXi-local adaptation, deliberate: the oracle
    ///   normalizes the non-differing branch with `X9` (BOM strip + CRLF→LF),
    ///   but LingXi's `Read` stores `decode_utf8_strict`
    ///   (`tools/file/src/shared.rs` — BOM strip ONLY, no CRLF collapse) and
    ///   `edit.rs` feeds that same raw-decoded form to the staleness
    ///   comparator. Collapsing CRLF here would make every CRLF memory file
    ///   fail `check_read_before_write`'s content-equality fallback forever.
    ///   The oracle's asymmetry IS preserved: normalize only on the
    ///   `!differs` branch; store `raw_content` verbatim on the `differs`
    ///   branch.
    /// - **`in_model_context`** carries the `...!jn && {contentNotInModelContext:!0}`
    ///   spread: LingXi's existing LRU-slot flag is that field's inverted
    ///   analog, already consumed by `model_context_keys()` /
    ///   `drain_model_context()`, so no entry-level twin is introduced.
    ///
    /// # Divergence (reason)
    /// The oracle also skips sentinel paths via `tHt` (`"<policyHelper>"`
    /// @226886661 / `"<managed-settings>"` @230811638). LingXi's loader emits
    /// only real walked filesystem paths — verified, no analog exists — so the
    /// skip is omitted rather than approximated with a `starts_with('<')`
    /// heuristic, which would be WIDER than the oracle.
    ///
    /// The oracle's AutoMem eviction pass (@245883373-655: delete seeded
    /// entries once memory-stores mode latches on) likewise has no LingXi
    /// analog — `ARe()` / `CLAUDE_MEMORY_STORES` is unported and there is no
    /// `AutoMem` tier. No latch is invented here.
    pub(super) async fn seed_memory_read_state(&self, files: &[crate::prompt::MemoryFile]) {
        for f in files {
            // The registry is keyed by CANONICAL paths: `FileReadTool` looks up
            // `canonicalize_and_validate(..)`'s output. The memory hierarchy's
            // `f.path` is built from the orchestrator's cwd verbatim, symlinks
            // and all — on macOS a `/var/...` cwd resolves to `/private/var/...`
            // — so seeding under the raw path silently never matches and the
            // dedup simply never fires. Canonicalize once here and use it for
            // BOTH the `has` guard and the key, or the two halves disagree.
            //
            // Falls back to the raw path if canonicalization fails; the file was
            // just read by the loader, so that is close to unreachable, and the
            // fallback is no worse than not seeding at all.
            let key = tokio::fs::canonicalize(&f.path)
                .await
                .unwrap_or_else(|_| f.path.clone());
            // `!readFileState.has(path)` — never clobber a real Read, never
            // MRU-promote (see `contains`' doc).
            if self
                .prompt_runtime
                .read_state_map
                .lock()
                .is_ok_and(|guard| guard.contains(&key))
            {
                continue;
            }
            let in_context = crate::prompt::memory_block::is_rendered_into_context(f);
            let now_ms = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis()),
            )
            .unwrap_or(i64::MAX);
            // `try{Ao = jn ? FQ(Fr.path) : Date.now()}catch{Ao = Date.now()}`.
            let mtime_ms = if in_context {
                match tokio::fs::metadata(&f.path)
                    .await
                    .and_then(|m| m.modified())
                {
                    Ok(t) => tool_api::read_file_state::mtime_ms_floor(t),
                    Err(_) => now_ms,
                }
            } else {
                now_ms
            };
            // `contentDiffersFromDisk ? (rawContent ?? content) : X9(content)`,
            // with LingXi's BOM-strip-only normalization (see the doc above).
            let content = if f.content_differs_from_disk {
                f.raw_content.clone()
            } else {
                f.raw_content
                    .strip_prefix('\u{feff}')
                    .unwrap_or(&f.raw_content)
                    .to_string()
            };
            tool_api::read_file_state::set_with_model_context(
                &self.prompt_runtime.read_state_map,
                key,
                tool_api::read_file_state::ReadFileEntry {
                    content,
                    mtime_ms,
                    offset: None,
                    limit: None,
                    // A seed is not a Read; the non-seeded dedup branch must
                    // never fire off it.
                    from_read: false,
                    seeded_from_context: in_context,
                    is_partial_view: f.content_differs_from_disk,
                },
                // ALWAYS false — deliberately NOT `in_context`.
                //
                // Two different axes that an earlier draft conflated:
                //   * `seeded_from_context` (above) is the oracle's `jn`/`MLu`
                //     and gates ONLY the seeded dedup stub.
                //   * this flag is LingXi's post-compact RESTORE set
                //     (`drain_model_context` -> `restore_post_compact_attachments`)
                //     and the "files touched this turn" input to
                //     `conditional_rules_reminder_message`.
                //
                // A memory file is re-injected by the SYSTEM PROMPT on every
                // turn, so enrolling it here would re-attach LINGXI.md after
                // every compaction and report it as touched. It belongs with the
                // host-seeded snapshots the drain doc already excludes.
                false,
            );
        }
    }

    /// Re-run a tool whose `can_use_tool` permission response was ORPHANED — the
    /// stdio control-plane received a late `control_response` it could not match
    /// to a pending request (a process restart with `--resume`, or a
    /// duplicate/late delivery). 1:1 with claude-code's `handleOrphanedPermission`
    /// (`queryHelpers.ts:224-343`), driven by the `mode:'orphaned-permission'`
    /// command (`print.ts:5291`).
    ///
    /// Locates the unresolved `tool_use` in the LIVE session history — which the
    /// CLI seeds from the transcript on `--resume`, so this mirrors claude-code's
    /// `findUnresolvedToolUse` over the transcript file while staying robust to
    /// compaction (a tool_use trimmed from the active context is not re-run) —
    /// forces the recovered permission `decision` past the gate, runs the tool
    /// through the SAME dispatch the normal turn loop uses, then appends +
    /// persists the resulting `tool_result`, completing the `tool_use →
    /// tool_result` chain.
    ///
    /// Returns `Ok(true)` when this id was handled (executed, OR short-circuited
    /// because its tool is no longer registered), `Ok(false)` when no unresolved
    /// `tool_use` with this id exists (already resolved, or absent). The caller
    /// records the id in its per-toolUseID "handled" set (twin of
    /// `handledOrphanedToolUseIds`) only on `Ok(true)`, so a not-found delivery
    /// can still recover later — matching claude-code (which adds to the set only
    /// when `findUnresolvedToolUse` succeeds).
    pub async fn run_orphaned_permission(
        &self,
        tool_use_id: &lingxi_core::types::ToolUseId,
        decision: lingxi_core::host::permission_gate::PermissionOutcome,
    ) -> Result<bool, OrchestratorError> {
        self.run_orphaned_permission_owned(tool_use_id, decision, None)
            .await
    }

    /// Replay a recovered tool under the host's retained operation owner. The
    /// normal dispatcher observes each tool's Cancel/Block policy; the host
    /// must await this entire recovery, including result persistence.
    pub async fn run_orphaned_permission_with_cancel(
        &self,
        tool_use_id: &lingxi_core::types::ToolUseId,
        decision: lingxi_core::host::permission_gate::PermissionOutcome,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<bool, OrchestratorError> {
        self.run_orphaned_permission_owned(tool_use_id, decision, Some(cancel))
            .await
    }

    async fn run_orphaned_permission_owned(
        &self,
        tool_use_id: &lingxi_core::types::ToolUseId,
        decision: lingxi_core::host::permission_gate::PermissionOutcome,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<bool, OrchestratorError> {
        use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};

        // 1. Locate the orphaned assistant message and confirm its `tool_use` is
        //    UNRESOLVED (no matching `tool_result`) — `findUnresolvedToolUse`
        //    (sessionStorage.ts:4478-4519). Snapshot a clone under the lock.
        let found = {
            let s = self.session.lock().await;
            find_unresolved_tool_use_in_history(&s.history, tool_use_id)
        };
        let Some(assistant_msg) = found else {
            // Already resolved (a tool_result exists) or never present — no-op,
            // exactly like claude-code's `findUnresolvedToolUse → null`.
            return Ok(false);
        };

        // Extract the matching `tool_use` block (queryHelpers.ts:238-251). The
        // lookup guarantees one exists; bail defensively otherwise.
        let Some((name, input, provider_id)) = (match &assistant_msg {
            ConversationMessage::Assistant { content, .. } => {
                content.iter().find_map(|b| match b {
                    ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        provider_id,
                    } if id == tool_use_id => {
                        Some((name.clone(), input.clone(), provider_id.clone()))
                    }
                    _ => None,
                })
            }
            _ => None,
        }) else {
            return Ok(false);
        };

        // Unknown-tool guard (queryHelpers.ts:256-259 `findToolByName → return`):
        // if the orphaned tool is no longer registered, emit NOTHING and push NO
        // tool_result, but still report recovery (`Ok(true)`) so the caller marks
        // this id handled — the TS sets `hasHandledOrphanedPermission` BEFORE
        // `handleOrphanedPermission` runs, so the gate is consumed even here.
        if self.find_dispatchable_tool(&name).is_none() {
            return Ok(true);
        }

        // 2. Re-emit the recovered assistant message as a self-contained stream
        //    frame (twin of `yield sdkAssistantMessage`, queryHelpers.ts:314-319)
        //    so a stream-json consumer sees the `tool_use` before its
        //    `tool_result`. Default-no-op sinks (TUI/tests) ignore the
        //    message_start/boundary envelope.
        {
            let model = self.session.lock().await.model.clone();
            let (msg_id, stop_reason) = match &assistant_msg {
                ConversationMessage::Assistant {
                    id, stop_reason, ..
                } => (id.to_string(), stop_reason.clone()),
                _ => (assistant_msg.id().to_string(), None),
            };
            self.output.emit_message_start(&msg_id, &model).await;
            if let ConversationMessage::Assistant { content, .. } = &assistant_msg {
                for b in content {
                    match b {
                        ContentBlock::Text { text } => self.output.emit_text(text).await,
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => self.output.emit_tool_call(id, name, input).await,
                        ContentBlock::Thinking {
                            thinking,
                            signature,
                        } => {
                            self.output
                                .emit_thinking(thinking, signature.as_deref())
                                .await;
                        }
                        _ => {}
                    }
                }
            }
            self.output
                .emit_assistant_message_identity(&assistant_msg.id())
                .await;
            self.output
                .emit_message_boundary(stop_reason.as_deref(), None)
                .await;
        }

        // 4. Apply `updatedInput` on an allow (queryHelpers.ts:262-276): an allow
        //    carries the host's possibly-rewritten input; a deny keeps the
        //    original (it will not run anyway).
        let (forced, final_input, permission_updates) = match decision {
            lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                updated_input,
                permission_updates,
                decision_classification: _,
            } => (
                crate::test_support::PermissionDecision::Allow,
                updated_input.unwrap_or(input),
                permission_updates,
            ),
            lingxi_core::host::permission_gate::PermissionOutcome::AllowAuto { updated_input } => {
                if let Err(error) = self.perms.set_permission_mode("auto").await {
                    // The current call was explicitly approved. Keep the one-shot
                    // allow, but never claim the live mode switched when the
                    // session-owned transition failed.
                    tracing::warn!(
                        %error,
                        "orphaned permission approved Auto mode but mode switch failed"
                    );
                }
                (
                    crate::test_support::PermissionDecision::Allow,
                    updated_input.unwrap_or(input),
                    Vec::new(),
                )
            }
            lingxi_core::host::permission_gate::PermissionOutcome::Deny { reason } => (
                crate::test_support::PermissionDecision::Deny { reason },
                input,
                Vec::new(),
            ),
        };
        if !permission_updates.is_empty() {
            self.perms.apply_permission_updates(&permission_updates);
            self.perms
                .persist_permission_updates(&permission_updates)
                .await;
        }

        // The orphaned assistant message is ALREADY in history (we found it
        // there), so — like claude-code's `alreadyPresent` guard
        // (queryHelpers.ts:299-312) — it is NOT re-pushed or re-persisted; only
        // the new `tool_result` below is appended.

        // 5. Force the recovered decision past the permission gate for this one
        //    `tool_use`, then run it through the SAME dispatch the normal turn
        //    loop uses (the `runTools` analog). The gate consumes (removes) the
        //    forced entry; clear any residue defensively.
        self.orphan_forced_decisions
            .lock()
            .await
            .insert(tool_use_id.clone(), forced);
        let tool_uses = vec![(tool_use_id.clone(), name, final_input, provider_id)];
        let dispatch_result =
            crate::turn_loop::dispatch_tool_uses_tracked(self, &tool_uses, cancel).await;
        self.orphan_forced_decisions
            .lock()
            .await
            .remove(tool_use_id);
        let (tool_results, _prevent, injected_messages, context_modifiers) = dispatch_result?;

        // 6. Append + persist the `tool_result` user message and any
        //    tool-injected follow-ups — mirroring the batched turn loop's
        //    post-dispatch block (`execute_one_turn`), the `recordTranscript`
        //    per-result twin (queryHelpers.ts:328-332).
        let tool_results_msg = ConversationMessage::User {
            id: MessageId::new(),
            content: tool_results,
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        {
            let mut s = self.session.lock().await;
            s.history.push(tool_results_msg.clone());
            for (m, source_id) in &injected_messages {
                s.history.push(m.clone());
                s.injected_message_sources.insert(m.id(), source_id.clone());
            }
        }
        self.persist_message_to_jsonl(&tool_results_msg).await;
        // O3: this recovery path dispatches exactly one tool — flush its hook
        // attachment lines after its tool_result, and skip the ephemeral
        // renderings (see the batched driver in `turn_loop.rs`).
        self.flush_hook_attachments(tool_use_id).await;
        for (m, _source_id) in &injected_messages {
            if m.is_meta() {
                continue;
            }
            self.persist_message_to_jsonl(m).await;
        }
        crate::turn_loop::apply_model_context_modifiers(self, context_modifiers).await;

        Ok(true)
    }

    /// Seed `read_file_state` for files surfaced as NESTED MEMORY — `k$o`'s
    /// `readFileState.set` half.
    ///
    /// Deliberately NOT [`Self::seed_memory_read_state`], which ports the
    /// EAGER-block seeding site (`xCt` @245883373). The two sites disagree on
    /// two fields, and the difference is load-bearing:
    ///
    /// | | eager (`xCt`) | nested (`k$o`) |
    /// |---|---|---|
    /// | `seededFromContext` | `MLu(file)` — only if actually rendered | `!0` always |
    /// | `timestamp` | mtime when rendered, else `Date.now()` | mtime always |
    ///
    /// A conditional rule is NOT in the eager block, so the eager site would
    /// seed it `seeded_from_context:false` — and the dedup stub would never
    /// fire for exactly the files this reminder just put in the context.
    pub(super) async fn seed_nested_memory_read_state(&self, files: &[crate::prompt::MemoryFile]) {
        for f in files {
            // Canonical key, lexical render — the fork that already shipped one
            // silent bug: `FileReadTool` looks up `canonicalize_and_validate`'s
            // output, so seeding under the raw path never matches on a macOS
            // `/var` -> `/private/var` cwd.
            let key = tokio::fs::canonicalize(&f.path)
                .await
                .unwrap_or_else(|_| f.path.clone());
            let now_ms = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis()),
            )
            .unwrap_or(i64::MAX);
            // `try{s=FQ(i.path)}catch{s=Date.now()}` — unconditional mtime.
            let mtime_ms = match tokio::fs::metadata(&f.path)
                .await
                .and_then(|m| m.modified())
            {
                Ok(t) => tool_api::read_file_state::mtime_ms_floor(t),
                Err(_) => now_ms,
            };
            let content = if f.content_differs_from_disk {
                f.raw_content.clone()
            } else {
                f.raw_content
                    .strip_prefix('\u{feff}')
                    .unwrap_or(&f.raw_content)
                    .to_string()
            };
            tool_api::read_file_state::set_with_model_context(
                &self.prompt_runtime.read_state_map,
                key,
                tool_api::read_file_state::ReadFileEntry {
                    content,
                    mtime_ms,
                    offset: None,
                    limit: None,
                    from_read: false,
                    seeded_from_context: true,
                    is_partial_view: f.content_differs_from_disk,
                },
                // ALWAYS false, for the reason spelled out on
                // `seed_memory_read_state`, plus one specific to this site: the
                // touched-file set is this reminder's own INPUT, so enrolling a
                // surfaced memory file would make it a trigger for the next
                // turn's discovery — a feedback loop walking its own ancestors.
                false,
            );
        }
    }

    /// Build the wire `tools` array for a turn from the registry's enabled tool
    /// set, serialized via [`tool_api::wire::tools_to_wire`] to the
    /// `{name, description, input_schema}` shape claude-code sends
    /// (`utils/api.ts:169-178`). Used by both the batched ([`execute_one_turn`])
    /// and streaming ([`Self::run_turn_streaming`]) paths.
    ///
    /// `ToolStaticContext::default()` (no feature flags) mirrors the system
    /// prompt's enable-filter punt; `include_examples: true` requests the full
    /// tool prompt as the `description`. The post-filter base wire schemas are
    /// cached per session by `(tool names, model, model_profile)`; per-turn
    /// dynamic fields are still applied after cloning the cached base. The wire
    /// order is parity-fixed by
    /// [`available_tools`](tool_api::ToolRegistry::available_tools): builtins
    /// `locale_cmp`-sorted as a contiguous prefix, then MCP / LSP / plugin
    /// tools `locale_cmp`-sorted — matching claude-code's `assembleToolPool` /
    /// `mergeAndFilterTools` (`tools.ts:345-367`, `utils/toolPool.ts:65-70`),
    /// which sort with `name.localeCompare`. `tools_to_wire` preserves that
    /// order. A session-level cache is a recommended follow-up.
    ///
    /// [`execute_one_turn`]: crate::turn_loop::execute_one_turn
    pub(crate) async fn build_wire_tools(&self) -> Vec<serde_json::Value> {
        use tool_api::tool_trait::PromptOptions;
        // claude-code builds the wire `tools` array with `prompt({model})`; the
        // session model gates model-dependent tool prompts (TodoWrite's
        // `Xla(model)=Dh(model)?FWd:UWd`). Snapshot it from the live session.
        let (model, model_profile) = {
            let s = self.session.lock().await;
            (s.model.clone(), s.model_profile.clone())
        };
        // Same snapshot, one turn earlier than it used to be taken: claude-code
        // registers the main-loop model as a LIVE accessor (`Dx(()=>HR(rt()))`,
        // read back by `J$e()`), so the `OO()` todo/task tool gate must see the
        // current `/model`. Republishing from THIS snapshot — rather than from
        // inside `filtered_available_tools` — keeps the advertise path free of a
        // second `session` lock, which would deadlock the callers that already
        // hold one.
        self.tools
            .set_main_loop_model(canonical_main_loop_model(&model));
        let tools = self.filtered_available_tools().await;
        // Upstream `nre`'s per-call conjunct: the Workflow description may only
        // point at the `workflow-authoring` skill when this request actually
        // offers the tool that loads it, and tool filtering can drop `Skill`.
        // Name-or-alias, matching `Wt(e, o)`.
        lingxi_core::host::session_flags::set_skill_tool_advertised(tools.iter().any(|tool| {
            tool.name() == tool_skill::skill::SKILL_TOOL_NAME
                || tool.aliases().contains(&tool_skill::skill::SKILL_TOOL_NAME)
        }));
        let cache_key = WireToolSchemaCacheKey {
            tool_names: tools.iter().map(|t| t.name().to_string()).collect(),
            dynamic_schema_revisions: tools
                .iter()
                .filter_map(|tool| {
                    tool.input_schema_revision()
                        .map(|revision| (tool.name().to_string(), revision))
                })
                .collect(),
            model: model.clone(),
            model_profile: model_profile.clone(),
            workflow_authoring_skill_reachable:
                lingxi_core::host::session_flags::workflow_authoring_skill_reachable(),
        };
        let mut wire = {
            let cached = self
                .prompt_runtime
                .wire_tool_schema_cache
                .lock()
                .await
                .clone();
            if let Some(cached) = cached.filter(|entry| entry.key == cache_key) {
                cached.wire
            } else {
                let wire = tool_api::wire::tools_to_wire(
                    &tools,
                    &PromptOptions {
                        include_examples: true,
                        model: Some(model.clone()),
                        model_profile: model_profile.clone(),
                    },
                )
                .await;
                *self.prompt_runtime.wire_tool_schema_cache.lock().await =
                    Some(WireToolSchemaCache {
                        key: cache_key.clone(),
                        wire: wire.clone(),
                    });
                wire
            }
        };
        // Structured-output strict mode (claude-code `tengu_structured_output_strict`
        // + `strictInputJSONSchema`): when the flag is on, mark the forced
        // `StructuredOutput` tool `strict` so the Anthropic codec sends its
        // schema in strict form. Default-OFF ⇒ no tool is marked ⇒ wire bytes
        // unchanged. Provider-gated downstream (only the Anthropic codec acts on
        // `strict`).
        if telemetry::flag_bool("tengu_structured_output_strict", false) {
            for t in &mut wire {
                if t.get("name").and_then(serde_json::Value::as_str)
                    == Some(crate::structured_output::STRUCTURED_OUTPUT_TOOL_NAME)
                {
                    if let Some(obj) = t.as_object_mut() {
                        obj.insert("strict".to_string(), serde_json::Value::Bool(true));
                    }
                }
            }
        }
        let tool_search_present = tools.iter().any(|tool| tool.name() == "ToolSearch");
        let has_deferred_candidates = tools.iter().any(|tool| {
            self.tools
                .deferral()
                .wants_defer(tool.name(), tool.should_defer())
        });
        // Keep ToolSearch available while an MCP server is still connecting,
        // even if the current catalog has no deferred definitions yet. Claude
        // does this so a model can retry discovery after the pending server
        // publishes its tools instead of permanently losing ToolSearch for the
        // turn/session.
        let has_pending_mcp_servers = if has_deferred_candidates {
            false
        } else if let Some(registry) = &self.mcp_registry {
            !registry.servers_pending().await.is_empty()
        } else {
            false
        };
        let request_supported = tool_search_present
            && (has_deferred_candidates || has_pending_mcp_servers)
            && tool_search_supported_for_request(&model, model_profile.as_deref());
        self.tools
            .deferral()
            .set_request_supported(request_supported);
        // Refresh the session-scoped tool-search gate (Claude Code `$U()`) for the
        // request builder now that the model/profile are resolved. Unlike
        // `request_supported`, `$U()` depends ONLY on the session mode + provider
        // support — never on a present `ToolSearch` tool or deferred candidates —
        // so side queries assembled with an empty toolset take the same
        // normalization branch as the main loop.
        lingxi_core::host::session_flags::set_tool_search_enabled(
            self.tools.deferral().mode().is_enabled()
                && tool_search_supported_for_request(&model, model_profile.as_deref()),
        );
        let complete_wire = wire.clone();
        // Tool Search (2.1.216): omit undiscovered deferred definitions and
        // stamp discovered definitions with `defer_loading: true`. The shared
        // `DeferralState` is owned by the registry and restored on resume, so a
        // prior ToolSearch result remains available without exposing the rest
        // of the deferred catalog.
        let context_window =
            compaction::context_window::context_window_for_model(&model, &self.api.active_betas());
        let exact_deferred_tokens = if self.tools.deferral().auto_percentage().is_some()
            && request_supported
            && has_deferred_candidates
        {
            let cached = self
                .prompt_runtime
                .deferred_tool_token_cache
                .lock()
                .await
                .get(&cache_key)
                .copied();
            if let Some(cached) = cached {
                cached
            } else {
                let deferred_names: std::collections::HashSet<&str> = tools
                    .iter()
                    .filter(|tool| {
                        self.tools
                            .deferral()
                            .wants_defer(tool.name(), tool.should_defer())
                    })
                    .map(|tool| tool.name())
                    .collect();
                let deferred_wire: Vec<serde_json::Value> = complete_wire
                    .iter()
                    .filter(|tool| {
                        tool.get("name")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|name| deferred_names.contains(name))
                    })
                    .cloned()
                    .collect();
                // Claude subtracts the fixed request/tool envelope from the
                // exact count. A zero response means the endpoint is
                // unavailable and selects the character fallback.
                let counted = match self
                    .api
                    .count_tokens_exact(
                        &model,
                        model_profile.as_deref(),
                        None,
                        Vec::new(),
                        deferred_wire,
                    )
                    .await
                {
                    Ok(Some(total)) if total != 0 => {
                        Some(total.saturating_sub(TOOL_TOKEN_COUNT_OVERHEAD))
                    }
                    Ok(_) | Err(_) => None,
                };
                self.prompt_runtime
                    .deferred_tool_token_cache
                    .lock()
                    .await
                    .insert(cache_key.clone(), counted);
                counted
            }
        } else {
            None
        };
        tool_api::wire::apply_defer_loading_with_context_and_tokens(
            &mut wire,
            &tools,
            self.tools.deferral(),
            context_window,
            exact_deferred_tokens,
        );
        // Auto mode learns whether it is active only after the schemas are
        // serialized and measured. Publish the complete deferred candidate
        // view (including descriptions), then hide ToolSearch itself whenever
        // the resolved request does not support the beta.
        self.tools
            .refresh_tool_search_view_from_wire(&complete_wire);
        if !self.tools.deferral().is_enabled() {
            wire.retain(|tool| {
                tool.get("name").and_then(serde_json::Value::as_str) != Some("ToolSearch")
            });
        }
        // Carved-slate keeps descriptions for already-inline tools stable
        // while allowing deferred tools to remain provider-discovered. Apply
        // the recorded map only to entries that are actually inline on this
        // request; the schema and defer markers stay live.
        self.apply_prompt_snapshot_tool_descriptions(&mut wire)
            .await;
        wire
    }

    async fn apply_prompt_snapshot_tool_descriptions(&self, wire: &mut [serde_json::Value]) {
        if !self.prompt_snapshot_eligible() {
            return;
        }
        let recorded = self
            .prompt_runtime
            .prompt_snapshot
            .lock()
            .await
            .as_ref()
            .filter(|snapshot| {
                !snapshot.system_prompt.is_empty()
                    && snapshot.system_prompt.iter().all(|part| !part.is_empty())
                    && snapshot.tools.iter().all(|tool| !tool.name.is_empty())
            })
            .map(|snapshot| {
                snapshot
                    .tools
                    .iter()
                    .map(|tool| (tool.name.clone(), tool.description.clone()))
                    .collect::<std::collections::HashMap<_, _>>()
            })
            .unwrap_or_default();
        if recorded.is_empty() {
            return;
        }
        for tool in wire {
            if tool
                .get("defer_loading")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            let Some(name) = tool.get("name").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(description) = recorded.get(name) else {
                continue;
            };
            if let Some(object) = tool.as_object_mut() {
                object.insert(
                    "description".to_string(),
                    serde_json::Value::String(description.clone()),
                );
            }
        }
    }

    /// `getTools`' (`iJ`) 2.1.238 tail block — the path that puts
    /// `WaitForMcpServers` in front of the model while MCP servers are still
    /// connecting:
    ///
    /// ```text
    /// if(eZf()&&!l.some((c)=>il(c,y0))&&!l.some((c)=>il(c,Qze)))l=[...l,...Ohe([Sdl],e)];
    /// ```
    ///
    /// (`cc-238.js @230759940`; `y0="ToolSearch"`, `Qze="WaitForMcpServers"`,
    /// `Ohe` = the deny-rule filter, `eZf(){return bdl(b7e()??[]).length>0}` =
    /// "at least one MCP client is `type === "pending"`"). 2.1.220's `d6` has no
    /// such block; the TOOL itself is not new (2.1.220 registers it too).
    ///
    /// The composite is: `WaitForMcpServers` is advertised iff at least one MCP
    /// server is still pending AND `ToolSearch` is not in the final list.
    ///
    /// The first half is `Sdl.isEnabled`'s `bdl(t).length>0` leg, delivered
    /// through the registry's synchronous pending mirror (refreshed here, read
    /// by `WaitForMcpServersTool::is_enabled` so `available_tools` still sorts
    /// the tool into the builtin prefix at its `locale_cmp` position). The
    /// second half is the tail block's `!l.some((c)=>il(c,y0))` guard, applied
    /// as a removal because `is_enabled` cannot see the rest of the list.
    ///
    /// `Ohe`'s deny filter runs over the whole list in the caller, so a deny
    /// rule naming `WaitForMcpServers` still removes it — no separate pass is
    /// needed for the appended tool the way the oracle needs `Ohe([Sdl],e)`.
    pub(super) fn apply_wait_for_mcp_servers_gate(
        tools: &mut Vec<std::sync::Arc<dyn tool_api::tool_trait::Tool>>,
    ) {
        // Name literals rather than `tool_mcp::…::WAIT_FOR_MCP_SERVERS_TOOL_NAME`
        // / `tool_meta::tool_search::TOOL_SEARCH_TOOL_NAME`: `orchestrator` does
        // not (and should not) depend on the tool crates. Pinned by
        // `tool_mcp::wait_for_mcp_servers::tests::name_is_byte_exact`.
        const WAIT_FOR_MCP_SERVERS: &str = "WaitForMcpServers";
        const TOOL_SEARCH: &str = "ToolSearch";

        if !tools.iter().any(|t| t.name() == WAIT_FOR_MCP_SERVERS) {
            return;
        }
        if tools.iter().any(|t| t.name() == TOOL_SEARCH) {
            tools.retain(|t| t.name() != WAIT_FOR_MCP_SERVERS);
        }
    }

    async fn filtered_available_tools(
        &self,
    ) -> Vec<std::sync::Arc<dyn tool_api::tool_trait::Tool>> {
        use tool_api::tool_trait::ToolStaticContext;

        // `eZf()` — refresh the registry's synchronous pending-server mirror
        // before the enable-filter runs, so `WaitForMcpServersTool::is_enabled`
        // observes the live state. See `apply_wait_for_mcp_servers_gate`.
        if let Some(registry) = self.mcp_registry.as_ref() {
            registry.refresh_pending_servers().await;
        }
        let mut tools = self.tools.available_tools(&ToolStaticContext::default());
        let denied = self.perms.tool_wide_deny_names().await;
        *self
            .tool_pool_denied_names
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = denied.clone();
        if !denied.is_empty() {
            tools.retain(|t| !self.is_tool_pool_denied(t.as_ref()));
        }
        Self::apply_wait_for_mcp_servers_gate(&mut tools);
        {
            let guard = self.lifecycle_runtime.main_thread_agent.read().await;
            if let Some(agent) = guard.as_ref() {
                if !agent.disallowed_tools.is_empty() {
                    let def_denied: std::collections::HashSet<&str> = agent
                        .disallowed_tools
                        .iter()
                        .map(|spec| spec.split('(').next().unwrap_or(spec).trim())
                        .collect();
                    tools.retain(|t| !def_denied.contains(t.name()));
                }
                match &agent.tool_policy {
                    agent::AgentToolPolicy::All { .. } => {}
                    agent::AgentToolPolicy::Explicit(names) => {
                        let allowed: std::collections::HashSet<&str> =
                            names.iter().map(String::as_str).collect();
                        tools.retain(|t| allowed.contains(t.name()));
                    }
                    agent::AgentToolPolicy::Except(names) => {
                        let blocked: std::collections::HashSet<&str> =
                            names.iter().map(String::as_str).collect();
                        tools.retain(|t| !blocked.contains(t.name()));
                    }
                }
                *self
                    .main_agent_tool_names
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(tools.iter().map(|tool| tool.name().to_string()).collect());
            } else {
                *self
                    .main_agent_tool_names
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            }
        }
        if self.is_coordinator_mode_enabled() {
            tools.retain(|t| self.is_coordinator_pool_tool(t.as_ref()));
        }
        tools
    }

    pub(super) fn text_content(message: &ConversationMessage) -> Option<String> {
        match message {
            ConversationMessage::User { content, .. } => content.iter().find_map(|block| {
                if let lingxi_core::types::ContentBlock::Text { text } = block {
                    Some(text.clone())
                } else {
                    None
                }
            }),
            _ => None,
        }
    }

    pub(super) fn is_mobile_runtime_environment_message(message: &ConversationMessage) -> bool {
        Self::text_content(message).is_some_and(|text| {
            text.starts_with("<system-reminder>\nMobile runtime environment (version ")
        })
    }

    /// Return the live, policy-filtered tool catalog in MCP's 2025-06-18
    /// `tools/list` shape. This deliberately reuses the normal model-facing
    /// schema builder so tool enablement, permission-wide denies, active-agent
    /// restrictions, descriptions, and ordering cannot drift between hosts.
    pub async fn mcp_tool_definitions(&self) -> Vec<serde_json::Value> {
        self.build_wire_tools()
            .await
            .into_iter()
            .filter_map(|wire| {
                let object = wire.as_object()?;
                let mut tool = serde_json::Map::new();
                tool.insert("name".into(), object.get("name")?.clone());
                if let Some(description) = object.get("description") {
                    tool.insert("description".into(), description.clone());
                }
                tool.insert(
                    "inputSchema".into(),
                    object
                        .get("input_schema")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({ "type": "object" })),
                );
                Some(serde_json::Value::Object(tool))
            })
            .collect()
    }

    /// Count the current model-visible MCP tools after the same filtering the
    /// turn prompt uses.
    pub(crate) async fn filtered_mcp_tool_count(&self) -> usize {
        self.filtered_available_tools()
            .await
            .into_iter()
            .filter(|tool| tool.is_mcp())
            .count()
    }

    /// Execute one host-originated tool request through the same dispatcher as
    /// a model-originated tool use: schema validation, PreToolUse hooks,
    /// permission policy, sandbox-backed tool execution, PostToolUse hooks, and
    /// result shaping. `None` identifies an unknown tool before dispatch.
    pub async fn call_tool_from_host(
        &self,
        tool_use_id: lingxi_core::types::ToolUseId,
        name: String,
        input: serde_json::Value,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Option<lingxi_core::types::ContentBlock>, OrchestratorError> {
        if self.find_dispatchable_tool(&name).is_none() {
            return Ok(None);
        }
        let tool_uses = vec![(tool_use_id, name, input, None)];
        let (mut results, _prevent_continuation, _injected, modifiers) =
            crate::turn_loop::dispatch_tool_uses_tracked(self, &tool_uses, cancel).await?;
        crate::turn_loop::apply_model_context_modifiers(self, modifiers).await;
        // O3: this host-driven path writes NO transcript line at all (the block
        // is handed back to the caller), so a queued hook attachment would be a
        // chain orphan. Drain it rather than leaving the entry in the map for
        // the life of the session.
        let _ = self.take_queued_hook_attachments(&tool_uses[0].0).await;
        Ok(results.pop())
    }
}

/// Port of claude-code `HR(e) = Xt(Ue(e,{identity:!0}))` — the canonicalisation
/// applied to the model setting before it becomes `mainLoopCanonical`.
///
/// `Ue(_,{identity:true})` resolves the setting to a concrete model id (the
/// port's `resolve_user_specified_model`, which also turns a bare `opus` /
/// `opusplan` / `sonnet` alias into a wire id), and `Xt(e)` strips a trailing
/// `[1m]` long-context suffix.
///
/// Deliberately NOT `lingxi_core::host::model_capabilities::normalize_model_id`:
/// that additionally strips `-eap` and dated/Bedrock/Vertex wrappers, which
/// `HR` does not. Feeding it to the gate would gate `claude-opus-5-eap`, an id
/// the oracle's `^claude-([a-z]+)-(\d+(?:-\d+)*)$` rejects and therefore
/// leaves enabled.
pub(crate) fn canonical_main_loop_model(raw: &str) -> Option<String> {
    let resolved = agent::model_resolution::resolve_user_specified_model(raw);
    let trimmed = resolved.trim();
    // `replace(/\[1m\]$/i, "")`.
    let base = if trimmed.to_lowercase().ends_with("[1m]") {
        trimmed[..trimmed.len() - "[1m]".len()].trim_end()
    } else {
        trimmed
    };
    (!base.is_empty()).then(|| base.to_string())
}
