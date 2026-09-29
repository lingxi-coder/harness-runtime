//! Per-turn reminders and memory/skill prefetch pipelines.

use super::*;

impl ConversationOrchestrator {
    /// `/brief` toggle reminder, consumed once by the next model call.
    ///
    /// The visible command status is rendered by the host immediately, while
    /// Claude Code also sends this transient meta message to the model so the
    /// next response uses the newly selected output channel. Startup
    /// `--brief` does not queue a reminder; only the interactive toggle does.
    pub(crate) fn brief_mode_reminder_message(&self) -> Option<ConversationMessage> {
        lingxi_core::host::session_flags::take_brief_mode_reminder()
            .map(|content| ConversationMessage::user_meta(MessageId::new(), content.to_string()))
    }

    /// OUTSTYLE.3: the byte-exact per-turn output-style reminder, or `None` when
    /// the default style is active.
    ///
    /// 1:1 with claude-code's `output_style` attachment. On EVERY turn where
    /// `settings.outputStyle != 'default'`, claude-code injects a meta user
    /// message into the model's input: `getOutputStyleAttachment`
    /// (`attachments.ts:1597-1612`) → `normalizeAttachmentForAPI`'s
    /// `'output_style'` case (`messages.ts:3797-3811`), which wraps
    /// `` `${outputStyle.name} output style is active. Remember to follow the
    /// specific guidelines for this style.` `` via `wrapInSystemReminder`
    /// (`messages.ts:3097-3099`, literally `` `<system-reminder>\n${content}\n</system-reminder>` ``).
    /// `outputStyle.name` is the builtin's `OUTPUT_STYLE_CONFIG[style].name`
    /// (`"Explanatory"` / `"Learning"`), here the resolved
    /// [`outputstyles::BuiltinOutputStyle::name`].
    ///
    /// Returns `None` for the `None`/`"default"`/unknown style (the same gate as
    /// the system-prompt section above), so the styleless path stays
    /// byte-identical and the locked turn-loop + streaming fixtures stay green.
    ///
    /// The reminder is a plain user-text [`ConversationMessage`] carrying the
    /// byte-exact string. The fresh [`MessageId`] is irrelevant: callers append
    /// this ONLY to the per-turn outgoing message snapshot, never to
    /// `session.history` nor JSONL, so it is TRANSIENT and never accumulates —
    /// its `isMeta` state is therefore immaterial (nothing persists it)
    /// (TS recomputes the attachment each turn — see `query.ts` mid-turn
    /// `getAttachmentMessages`). Position mirrors TS: the caller appends it as a
    /// trailing meta user message after the user prompt / tool-results
    /// (`processTextPrompt` returns `[userMessage, ...attachmentMessages]`;
    /// `query.ts:1580-1590` pushes the attachment after `toolResults`).
    pub(crate) async fn output_style_reminder_message(&self) -> Option<ConversationMessage> {
        let resolved = self.resolve_active_output_style().await?;
        // 2.1.238 renderer (`Cqm.output_style`, table @296733172):
        //
        // ```js
        // output_style:(e)=>{if(typeof e.style!=="string"||e.style==="")return[];
        //  if(e.style.length>gFn)return T(`Output style name exceeds ${gFn} characters (${e.style.length}); suppressing its per-turn reminder`,{level:"error"}),[];
        //  return Zy([kn({content:`${pze(e.style)} output style is active. ${e.turnReminder??"Remember to follow the specific guidelines for this style."}`,isMeta:!0})])},
        // ```
        //
        // The empty-name arm is already covered by `resolve_active_output_style`
        // (`None` for default/unknown). `gFn = 256` (@285128933) and the `pze`
        // escape are new in 2.1.238; `e.style.length` is UTF-16 code units.
        let name = resolved.name.as_str();
        if name.is_empty() {
            return None;
        }
        let name_len = name.encode_utf16().count();
        if name_len > crate::prompt::sanitize::MAX_OUTPUT_STYLE_NAME_LEN {
            tracing::error!(
                "Output style name exceeds {} characters ({name_len}); suppressing its per-turn reminder",
                crate::prompt::sanitize::MAX_OUTPUT_STYLE_NAME_LEN
            );
            return None;
        }
        // The renderer is
        // `` `${escape(style)} output style is active. ${e.turnReminder ??
        //    "Remember to follow the specific guidelines for this style."}` ``.
        //
        // This file used to argue at length that the fallback was the ONLY
        // reachable arm, because the two styles carrying a `turnReminder`
        // (`Proactive`, `Concise`) were not ported. They are now, so the arm is
        // live and the style's own sentence wins.
        //
        // A DISK or PLUGIN style still renders the fallback: `turnReminder` is
        // not a frontmatter key upstream, so a file cannot supply one — see
        // `ResolvedOutputStyle::turn_reminder`.
        let reminder = resolved
            .turn_reminder
            .unwrap_or("Remember to follow the specific guidelines for this style.");
        let content = format!(
            "<system-reminder>\n{} output style is active. {reminder}\n</system-reminder>",
            crate::prompt::sanitize::escape_reminder_text(name)
        );
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// SKILLLIST.1: the per-turn, transient `skill_listing` reminder, or `None`
    /// when no provider is wired, the `Skill` tool is absent this turn, or there
    /// are no model-invocable skills.
    ///
    /// 1:1 with claude-code's `skill_listing` attachment: `getSkillToolCommands`
    /// (`commands.ts:565`) selects the eligible skills, `formatCommandsWithinBudget`
    /// (`SkillTool/prompt.ts`) renders them within a ~1%-of-context char budget,
    /// and `normalizeAttachmentForAPI`'s `'skill_listing'` case
    /// (`messages.ts:3728-3738`) wraps the body in a `<system-reminder>` meta
    /// user message: `"The following skills are available for use with the Skill
    /// tool:\n\n{listing}"`. The Skill-tool gate mirrors `attachments.ts:2668`.
    ///
    /// Like the OUTSTYLE.3 reminder, the message is appended ONLY to the per-turn
    /// outgoing snapshot (never to `session.history` / JSONL), so it is recomputed
    /// each turn and never accumulates. `None` keeps the styleless/skilless path
    /// byte-identical and the locked fixtures green.
    ///
    /// DELTA (SKILLLIST.1): turn-0 emits the FULL listing; each later turn emits
    /// ONLY skills that have NOT appeared in a prior turn's reminder, tracked via
    /// [`Self::sent_skill_names`]. When no new skill appears, returns `None` (no
    /// reminder that turn). 1:1 with TS `sentSkillNames` (attachments.ts:2607,
    /// 2699): the budgeter still runs over the delta subset, so the rendered
    /// bytes match what TS would send for that turn's new-skill set.
    /// The per-turn, transient plan-mode reminder (206 `plan_mode` attachment,
    /// builder `xEg`), or `None` when plan mode is not active.
    ///
    /// 1:1 with the binary's `xEg(t)`: gated on `permissionMode === "plan"`
    /// (here `session.plan_mode`), it returns the `{type:"plan_mode",
    /// reminderType, isSubAgent, planFilePath, planExists, ...customInstructions}`
    /// attachment which `KJn` assembles (`l=await xEg(t)`) BEFORE the
    /// invoked-skills bodies (`REg`) and the tool/mcp deltas (`gYt`/`YJn`), i.e.
    /// before the skill-listing reminder in the port's per-turn sequence.
    ///
    /// CADENCE (2.1.238 `X4T` @296525982, `txl` @296558044 — see
    /// [`PlanReminderCadence`]): at most ONE plan-mode reminder per
    /// [`PLAN_TURNS_BETWEEN_ATTACHMENTS`] real user turns, and every
    /// [`PLAN_FULL_REMINDER_EVERY_N_ATTACHMENTS`]th emitted attachment is the
    /// FULL body (`c % 5 === 1` ⇒ #1, #6, #11 … full; the rest sparse). The port
    /// previously emitted on EVERY model call, full exactly once and sparse
    /// forever after — which both over-fired and never returned to the full body.
    ///
    /// `isSubAgent` is ALWAYS `false` here: subagents never run through
    /// `ConversationOrchestrator` (every orchestrator is a depth-0 main thread),
    /// so the `H5T` variant is unreachable via this path. Appended ONLY to the
    /// per-turn OUTGOING snapshot (never `session.history` / JSONL) so it never
    /// accumulates; `None` keeps the locked turn-loop fixtures byte-identical
    /// (default: plan mode OFF).
    /// Upstream returns a LIST here (`J_s`): a `plan_mode_reentry` attachment
    /// may precede the `plan_mode` one on the entry that finds an existing plan
    /// file, and both sit behind the SAME cadence gate — `lyr`'s early return
    /// runs before the reentry push, so a suppressed turn emits neither.
    pub(crate) async fn plan_mode_turn_messages(&self) -> Vec<ConversationMessage> {
        let (path, exists, real_user_turns, entered_plan_mode, reentry) = {
            let mut s = self.session.lock().await;
            if !s.plan_mode {
                return Vec::new();
            }
            let path = self.session_plan_file_path(&s.session_id);
            let exists = std::path::Path::new(&path).exists();
            // `ixl`'s turn counter: non-meta user messages carrying NO
            // `tool_result` block. Tool-result continuations within one turn are
            // NOT turns, so the cadence gate holds the reminder for the whole
            // multi-step turn rather than re-firing on every model call.
            let real_user_turns = s
                .history
                .iter()
                .filter(|m| match m {
                    ConversationMessage::User {
                        content,
                        is_meta: false,
                        ..
                    } => !content
                        .iter()
                        .any(|b| matches!(b, lingxi_core::types::ContentBlock::ToolResult { .. })),
                    _ => false,
                })
                .count();
            // `plan_reminder_shown == false` marks a fresh plan-mode ENTRY
            // (`EnterPlanMode` / `handle_impl` clear it), i.e. the
            // `plan_mode_exit` boundary `Y4T` stops counting at.
            let entered_plan_mode = !s.plan_reminder_shown;
            s.plan_reminder_shown = true;
            // `if(nPt()&&y!==null){C.push({type:"plan_mode_reentry",…}),NM(!1)}`
            // — one reentry reminder per exit→enter cycle, and only when a plan
            // file from the previous session is actually on disk. A missing file
            // leaves the flag set for the next entry, exactly as upstream does.
            let reentry = s.plan_mode_exited && exists;
            if reentry {
                s.plan_mode_exited = false;
            }
            (path, exists, real_user_turns, entered_plan_mode, reentry)
        };
        // Decide emission + full/sparse under the cadence lock so two concurrent
        // turns cannot both render attachment `c`.
        let sparse = {
            let mut c = self.prompt_runtime.plan_reminder_cadence.lock().await;
            if entered_plan_mode {
                *c = PlanReminderCadence::default();
            }
            if let Some(last) = c.real_user_turns_at_last_emission {
                // `if(_ && y < TURNS_BETWEEN_ATTACHMENTS) return []`
                if real_user_turns.saturating_sub(last) < PLAN_TURNS_BETWEEN_ATTACHMENTS {
                    return Vec::new();
                }
            }
            c.attachments_emitted += 1;
            c.real_user_turns_at_last_emission = Some(real_user_turns);
            // `c % FULL_REMINDER_EVERY_N_ATTACHMENTS === 1 ? "full" : "sparse"`
            c.attachments_emitted % PLAN_FULL_REMINDER_EVERY_N_ATTACHMENTS != 1
        };
        let params = crate::prompt::plan_reminder::PlanReminderParams {
            plan_file_path: &path,
            plan_exists: exists,
            // C5: `--plan-mode-instructions` custom workflow body (borrows from
            // `self.config`, which outlives `params`; the session guard is already
            // dropped). `None` ⇒ the default 5-phase reminder.
            custom_instructions: self.config.plan_mode_instructions.as_deref(),
            is_subagent: false,
            reminder_type_sparse: sparse,
            // `zx()==="default"` — an unset `output_style` IS the default style.
            output_style_is_default: self
                .config
                .output_style
                .as_deref()
                .map_or(true, |style| style == "default"),
        };
        // All three plan-mode renderers (`M5T` full / `L5T` sparse / `H5T`
        // subagent) return through the batch wrapper `Zy` (2.1.238 @296675470),
        // which maps `NT` = `` `<system-reminder>\n${e}\n</system-reminder>` ``
        // (@296673554) over every message and marks it `isMeta:!0`. The body
        // renderer stays pure (its byte-exact unit tests pin the bare body); the
        // envelope is applied here, exactly as the other per-turn reminders do.
        let body = crate::prompt::plan_reminder::render_plan_mode_reminder(&params);
        let content = format!("<system-reminder>\n{body}\n</system-reminder>");
        let mut out = Vec::with_capacity(2);
        if reentry {
            let reentry_body = crate::prompt::plan_reminder::render_plan_mode_reentry(&path);
            out.push(ConversationMessage::user_meta(
                MessageId::new(),
                format!("<system-reminder>\n{reentry_body}\n</system-reminder>"),
            ));
        }
        out.push(ConversationMessage::user_meta(MessageId::new(), content));
        out
    }

    /// The `plan_mode_exit` reminder — 2.1.266 `Z_s`:
    ///
    /// ```js
    /// async function Z_s(e,n){if(ue(n).mode==="plan")return Vz(!1),[];
    ///   let{foundPlanModeAttachment:r}=lyr(e??[]);
    ///   if(!n$n()&&!r)return[];
    ///   Vz(!1);
    ///   let o=ay(n.agentId),d=zF(n.agentId)!==null;
    ///   return[{type:"plan_mode_exit",planFilePath:o,planExists:d}]}
    /// ```
    ///
    /// Still in plan mode ⇒ clear the pending flag and emit nothing. Otherwise
    /// emit when the flag is set. The `!r` half of upstream's guard (a
    /// `plan_mode` attachment still visible in history even with no pending
    /// flag) is not reproduced: LingXi's plan-mode reminders live only in the
    /// per-turn outgoing snapshot, never in `session.history`, so there is no
    /// history to scan — the flag is the only witness.
    pub(crate) async fn plan_mode_exit_message(&self) -> Option<ConversationMessage> {
        let (path, exists) = {
            let mut s = self.session.lock().await;
            if s.plan_mode {
                s.plan_mode_exit_pending = false;
                return None;
            }
            if !s.plan_mode_exit_pending {
                return None;
            }
            s.plan_mode_exit_pending = false;
            let path = self.session_plan_file_path(&s.session_id);
            let exists = std::path::Path::new(&path).exists();
            (path, exists)
        };
        let body = crate::prompt::plan_reminder::render_plan_mode_exit(&path, exists);
        Some(ConversationMessage::user_meta(
            MessageId::new(),
            format!("<system-reminder>\n{body}\n</system-reminder>"),
        ))
    }

    pub(crate) async fn skill_listing_reminder_message(&self) -> Option<ConversationMessage> {
        let provider = self.prompt_runtime.skill_listing.as_ref()?;
        // Gate on the Skill tool being available this turn (attachments.ts:2668).
        self.find_dispatchable_tool("Skill")?;
        let entries = provider.skill_entries().await;

        // DELTA: keep only skills not yet sent this session, then record them as
        // sent. Turn 0 keeps everything (the set is empty); subsequent turns keep
        // only newly-appeared names. An empty delta ⇒ no reminder this turn.
        let new_entries: Vec<crate::prompt::skill_listing::SkillListingEntry> = {
            let mut sent = self.prompt_runtime.sent_skill_names.lock().await;
            let delta: Vec<_> = entries
                .into_iter()
                .filter(|e| !sent.contains(&e.name))
                .collect();
            for e in &delta {
                sent.insert(e.name.clone());
            }
            delta
        };
        if new_entries.is_empty() {
            return None;
        }

        // ~1% of the active model's context window (TS getCharBudget). Resolved
        // with no betas — the small 200k↔1M budget delta only matters past ~30
        // skills, where the budgeter degrades gracefully. Read the LIVE model
        // (mutated by /model + resume), not the frozen boot `config.model`, so a
        // switch across a 200k↔1M window boundary re-sizes the budget correctly
        // (mirrors `build_prompt_context`).
        let model = self.session.lock().await.model.clone();
        let window =
            compaction::context_window::context_window_for_model(&model, &self.api.active_betas())
                as usize;
        let content = crate::prompt::skill_listing::render_reminder(&new_entries, Some(window))?;
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// The per-turn, transient `async_hook_response` reminder, or `None` when no
    /// source is wired or no background (`async`) hook has completed since the
    /// last turn.
    ///
    /// 1:1 with claude-code's `async_hook_response` attachment
    /// (`getAsyncHookResponseAttachments`, attachments.ts:3464 →
    /// `normalizeAttachmentForAPI`, messages.ts:4026): drains the completed
    /// background-hook responses (CONSUME-ONCE — TS `removeDeliveredAsyncHooks`)
    /// and wraps their `system_message` text (which already folds in any
    /// `additionalContext`) in one `<system-reminder>` meta user message. Like
    /// the skill-/agent-listing reminders it is appended ONLY to the per-turn
    /// OUTGOING snapshot, never `session.history` / JSONL, so it never
    /// accumulates. No delta set is needed — draining the source IS the dedup.
    pub(crate) async fn async_hook_response_reminder_message(&self) -> Option<ConversationMessage> {
        let provider = self.prompt_runtime.async_hook_responses.as_ref()?;
        let responses = provider.take_pending_responses().await;
        let content = crate::prompt::async_hook_response::render_reminder(&responses)?;
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// T35: the per-turn `task-notification` reminders — one message per
    /// completion, empty when no source is wired or no background task finished
    /// since the last turn.
    ///
    /// Mirrors [`Self::async_hook_response_reminder_message`]: drains the
    /// registry's terminal-not-notified tasks (CONSUME-ONCE — the registry marks
    /// each `notified` + evicts on drain) and renders their `<task-notification>`
    /// blocks (claude-code's per-task-type `enqueue*Notification` formats) inside
    /// one `<system-reminder>` meta user message PER completion, each whose
    /// first line is the
    /// `NON_USER_INPUT_HEADER` provenance header (2.1.238 `b_a` @285068292,
    /// applied to every `task-notification`-origin user message so the model
    /// never treats a machine-generated completion as user consent; it also
    /// escapes any literal `</system-reminder>` in the task output so a task
    /// cannot close the envelope early). Appended ONLY to the
    /// per-turn OUTGOING snapshot, never `session.history` / JSONL, so it never
    /// accumulates. No delta set is needed — draining the registry IS the dedup.
    ///
    /// REM-14 side effect: a terminal `dream` task in the drained batch is the
    /// port's only signal that the BACKGROUND MEMORY CONSOLIDATOR finished, so
    /// this is where the oracle's `setAppState({pendingMemoryUpdates:[…]})`
    /// enqueue lands. The queue is drained separately by
    /// [`Self::memory_update_reminder_messages`].
    ///
    /// Both turn drivers reach the drain through
    /// [`Self::task_notification_reminder_messages_in_turn`] (via the shared
    /// collector), which is where `in_human_turn` comes from; this
    /// `in_human_turn = false` wrapper has no production caller left and is
    /// kept for tests that drain the registry directly.
    ///
    /// `#[cfg(test)]` so that stays true: without it the method is dead in a
    /// non-test build (it has warned since the collector landed), and a future
    /// production caller would silently contradict the paragraph above instead
    /// of failing to compile.
    #[cfg(test)]
    pub(crate) async fn task_notification_reminder_messages(&self) -> Vec<ConversationMessage> {
        self.task_notification_reminder_messages_in_turn(false)
            .await
    }

    pub(crate) async fn task_notification_reminder_messages_in_turn(
        &self,
        in_human_turn: bool,
    ) -> Vec<ConversationMessage> {
        let Some(provider) = self.prompt_runtime.task_notifications.as_ref() else {
            return Vec::new();
        };
        let notifications = provider.take_pending_task_notifications().await;
        self.enqueue_memory_updates_from(&notifications);
        // ONE message per completion: claude-code's `ha(…)` enqueue runs once
        // per notification and the envelope is applied per message, so two
        // tasks finishing in the same turn are two user messages. Folding them
        // into one envelope also folded two provenance headers into one.
        crate::prompt::task_notification::render_reminders_in_turn(&notifications, in_human_turn)
            .into_iter()
            .map(|content| ConversationMessage::user_meta(MessageId::new(), content))
            .collect()
    }

    /// Cap on [`Self::pending_memory_updates`]; oldest entries are dropped.
    ///
    /// Enqueue and drain are separate halves that used to sit on different
    /// drivers — the batched path enqueued without ever draining, so the queue
    /// could only grow. Both drivers now share one collector
    /// (`conversation/drivers/prepare.rs`), which drains a few lines after it
    /// enqueues, so in production the queue is emptied every model step. The
    /// bound stays as defence in depth: [`Self::enqueue_memory_updates_from`]
    /// is reached from
    /// [`Self::task_notification_reminder_messages_in_turn`], and a caller that
    /// drains notifications OUTSIDE the collector still enqueues here with
    /// nothing to empty it.
    pub(crate) const MAX_PENDING_MEMORY_UPDATES: usize = 8;

    /// REM-14 enqueue half: turn every terminal `dream` notification into a
    /// [`crate::prompt::memory_update::PendingMemoryUpdate`].
    ///
    /// `summary` is the dream agent's own final text (`result`) — the very
    /// thing `tasks/src/handlers/dream.rs`'s prompt asks it to return ("Return a
    /// brief summary of what you consolidated, updated, or pruned") — falling
    /// back to the task description when the agent returned nothing. A `failed`
    /// / `killed` dream is skipped: nothing was consolidated.
    pub(super) fn enqueue_memory_updates_from(
        &self,
        notifications: &[lingxi_core::host::task_registry::TaskNotification],
    ) {
        let fresh: Vec<crate::prompt::memory_update::PendingMemoryUpdate> = notifications
            .iter()
            .filter(|n| n.task_type == "dream" && n.status == "completed")
            .map(|n| crate::prompt::memory_update::PendingMemoryUpdate {
                source: crate::prompt::memory_update::MemoryUpdateSource::Dream,
                summary: n
                    .result
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(n.description.as_str())
                    .to_string(),
            })
            .collect();
        if fresh.is_empty() {
            return;
        }
        let mut queue = self
            .prompt_runtime
            .pending_memory_updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.extend(fresh);
        let overflow = queue.len().saturating_sub(Self::MAX_PENDING_MEMORY_UPDATES);
        if overflow > 0 {
            queue.drain(..overflow);
        }
    }

    /// REM-14 — the per-turn `memory_update` reminders: one meta message per
    /// queued background memory consolidation.
    ///
    /// 1:1 with the oracle producer `jzm(e)` @**296554545**:
    ///
    /// ```js
    /// function jzm(e){let t=e.getAppState().pendingMemoryUpdates;if(t.length===0)return[];
    ///   e.setAppState(…clear…);
    ///   let n=…memory index path…, o=XAa(e.session),
    ///       i=(s)=>s===n||o.has(s)||e.readFileState.has(s)||e.loadedNestedMemoryPaths?.[s]===!0;
    ///   return t.map((s)=>({type:"memory_update",source:s.source,summary:s.summary,
    ///                       paths:s.paths,inContextPaths:s.paths.filter(i)}))}
    /// ```
    ///
    /// * DRAIN — consume-once, exactly like the oracle's clear-on-read.
    /// * `paths` — the oracle's writer reports them; the port recomputes them by
    ///   listing the user memdir (the same directory the `# Memory` section
    ///   points the model at, `memory_prefetch.user_memdir()`) for entries whose
    ///   mtime is newer than the previous scan. `None` when no memdir is wired —
    ///   which is also when the memory feature itself is off, so nothing can have
    ///   been consolidated.
    /// * `inContextPaths` — [`crate::prompt::memory_update::select_in_context_paths`]
    ///   over `readFileState.has(s)`, the one arm of the oracle's `i` predicate
    ///   the port has (there is no separate memory-index file or session-memory
    ///   set here).
    /// * RENDER — [`crate::prompt::memory_update::render_memory_update`], wrapped
    ///   per update, matching `Zy([kn({content:o.join("\n"),isMeta:!0})])`.
    ///
    /// Silent in a stock session: nothing queues unless a `dream` task completes.
    pub(crate) async fn memory_update_reminder_messages(&self) -> Vec<ConversationMessage> {
        let pending: Vec<crate::prompt::memory_update::PendingMemoryUpdate> = {
            let mut queue = self
                .prompt_runtime
                .pending_memory_updates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *queue)
        };
        if pending.is_empty() {
            return Vec::new();
        }
        let Some(memdir) = self
            .prompt_runtime
            .memory_prefetch
            .as_ref()
            .and_then(|p| p.user_memdir())
            .map(std::path::Path::to_path_buf)
        else {
            return Vec::new();
        };

        // Which memdir files moved since the last scan.
        let since = self
            .prompt_runtime
            .last_memory_scan_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let now_ms = tool_api::read_file_state::mtime_ms_floor(std::time::SystemTime::now());
        self.prompt_runtime
            .last_memory_scan_ms
            .store(now_ms, std::sync::atomic::Ordering::Relaxed);
        let mut paths: Vec<String> = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&memdir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                let Ok(meta) = entry.metadata().await else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let Ok(modified) = meta.modified() else {
                    continue;
                };
                if tool_api::read_file_state::mtime_ms_floor(modified) > since {
                    paths.push(path.to_string_lossy().into_owned());
                }
            }
        }
        paths.sort();

        let in_context = {
            let guard = self
                .prompt_runtime
                .read_state_map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::prompt::memory_update::select_in_context_paths(&paths, |p| {
                guard.contains(std::path::Path::new(p))
            })
        };

        pending
            .into_iter()
            .map(|queued| {
                let body = crate::prompt::memory_update::render_memory_update(
                    &crate::prompt::memory_update::MemoryUpdate {
                        source: queued.source,
                        summary: queued.summary,
                        paths: paths.clone(),
                        in_context_paths: in_context.clone(),
                    },
                );
                ConversationMessage::user_meta(
                    MessageId::new(),
                    format!("<system-reminder>\n{body}\n</system-reminder>"),
                )
            })
            .collect()
    }

    /// Finding #73: the per-turn `todo_reminder` (V1) / `task_reminder` (V2)
    /// meta user message, or `None` when not eligible this turn.
    ///
    /// 1:1 with the binary's producer `()=>TE()?B4p(o,t):M4p(o,t)` (`ytl`,
    /// offset ~203087213). [`tool_task::reminder::select_mode`] picks V1 vs V2
    /// (`TE()`). For the selected variant this mirrors the `M4p`/`B4p` gates:
    /// 1. killswitch `wgo()!=="off"`;
    /// 2. the `Brief` tool (`rjn`/`SendUserMessage`) is ABSENT (present ⇒ skip);
    /// 3. the relevant tool is PRESENT this turn — `TodoWrite` (V1) /
    ///    `TaskUpdate` (V2);
    /// 4. the history is non-empty (`!e||e.length===0 ⇒ []`);
    /// 5. BOTH counters reach their thresholds (`turns_since_last_todo_write >=
    ///    TURNS_SINCE_WRITE && turns_since_last_reminder >= TURNS_BETWEEN_REMINDERS`).
    ///
    /// On fire it renders the body inside a `<system-reminder>` envelope as a
    /// META user message — the oracle's `Zy([kn({content:o,isMeta:!0})])`
    /// (2.1.238 @296690005 / @296690634, `Zy` @296675470 mapping `NT`
    /// @296673554) — and RESETS `turns_since_last_reminder` to
    /// `0`. V1 reads `session.todos`; V2 reads the wired
    /// [`crate::prompt::todo_reminder::TodoReminderTaskProvider`] (no provider ⇒
    /// base text only, an empty store). Appended ONLY to the per-turn OUTGOING
    /// snapshot (never `session.history` / JSONL) so it never accumulates.
    ///
    /// COUNTER NOTE: the binary recomputes the counters by scanning the message
    /// log for the last `TodoWrite`/`Task` tool_use and the last reminder
    /// ATTACHMENT. This engine never persists the reminder attachment, so the
    /// counters are tracked as explicit `SessionState` fields, incremented once
    /// per assistant turn (`bump_reminder_turn_counters`) and reset on the
    /// relevant tool call (`note_todo_reminder_tool_call`).
    pub(crate) async fn todo_reminder_message(&self) -> Option<ConversationMessage> {
        // (1) killswitch.
        if tool_task::reminder::is_killswitched() {
            return None;
        }
        // (2) Brief (`SendUserMessage`/`Brief`) present ⇒ skip (both variants).
        if self.find_dispatchable_tool("SendUserMessage").is_some() {
            return None;
        }

        // (2b) the `OO()` model gate. `select_mode` returns `None` when the
        // todo/task tools have been withdrawn, porting BOTH oracle guards
        // (`if(X_()||!OO())return[]` for V1, `if(!h3())return[]` for V2) — the
        // reminder must not describe tools the model was never offered. The
        // canonical main-loop model is the one the registry publishes, so this
        // and `available_tools` cannot disagree.
        let Some(mode) = tool_task::reminder::select_mode(self.tools.main_loop_model().as_deref())
        else {
            return None;
        };

        // (3) tool-presence gate + (4) non-empty history + (5) counters, all
        // read under one session lock so the snapshot is consistent. We reset
        // `turns_since_last_reminder` here (inside the lock) iff we fire.
        let mut s = self.session.lock().await;

        // (4) empty history ⇒ no reminder.
        if s.history.is_empty() {
            return None;
        }
        // (5) both thresholds.
        if s.turns_since_last_todo_write < tool_task::reminder::TURNS_SINCE_WRITE
            || s.turns_since_last_reminder < tool_task::reminder::TURNS_BETWEEN_REMINDERS
        {
            return None;
        }

        match mode {
            tool_task::reminder::ReminderMode::V1Todo => {
                // (3) TodoWrite must be present this turn.
                self.find_dispatchable_tool("TodoWrite")?;
                let items: Vec<(lingxi_core::TodoState, String)> = s
                    .todos
                    .iter()
                    .map(|t| (t.status, t.content.clone()))
                    .collect();
                s.turns_since_last_reminder = 0;
                drop(s);
                // `case"todo_reminder"` returns `Zy([kn({content:o,isMeta:!0})])`
                // (2.1.238 @296690005), i.e. the body wrapped by `NT` =
                // `` `<system-reminder>\n${e}\n</system-reminder>` `` and marked
                // meta. The body renderer stays pure (byte-locked in
                // `tool_task::reminder`); the envelope is applied here.
                let body = tool_task::reminder::render_v1(&items);
                let content = format!("<system-reminder>\n{body}\n</system-reminder>");
                Some(ConversationMessage::user_meta(MessageId::new(), content))
            }
            tool_task::reminder::ReminderMode::V2Task => {
                // (3) TaskUpdate must be present this turn.
                self.find_dispatchable_tool("TaskUpdate")?;
                let session_id = s.session_id;
                s.turns_since_last_reminder = 0;
                drop(s);
                // Read the V2 task store outside the session lock.
                let items: Vec<(String, lingxi_core::TodoState, String)> =
                    match &self.prompt_runtime.todo_reminder_tasks {
                        Some(provider) => provider
                            .task_items(session_id)
                            .await
                            .into_iter()
                            .map(|t| (t.id, t.status, t.subject))
                            .collect(),
                        None => Vec::new(),
                    };
                // `case"task_reminder"` — same `Zy([kn({…,isMeta:!0})])` envelope
                // as the V1 branch (2.1.238 @296690634).
                let body = tool_task::reminder::render_v2(&items);
                let content = format!("<system-reminder>\n{body}\n</system-reminder>");
                Some(ConversationMessage::user_meta(MessageId::new(), content))
            }
        }
    }

    /// `date_change` (cc `Cop` + renderer `date_change:` in the attachment
    /// table): a session that crosses local midnight tells the model the new
    /// date once per changed date. Producer logic 1:1 —
    /// `wcs()` = local `YYYY-MM-DD` ([`crate::prompt::env_meta::current_date_string`]),
    /// `LGe()` = the memoized session-start date; equal ⇒ no attachment, and an
    /// already-DELIVERED reminder for the same `newDate` dedupes.
    ///
    /// PURE — the dedupe is advanced by [`Self::commit_date_change_reminder`]
    /// once the request carrying the reminder has actually been issued. The
    /// oracle can latch on produce because it materialises the attachment as a
    /// real message (`Va(c,o)`) and pushes it into the message array BEFORE the
    /// call, so its dedupe reads the same fact it delivered; the port's
    /// reminder lives only in the outgoing snapshot, so a step that ends before
    /// the call (blocking-limit preempt, stream error, abort) must not consume
    /// it.
    ///
    /// Rendered through `pm([zr({content, isMeta:!0})])` = `<system-reminder>`
    /// wrap + meta user message, appended to THIS turn's OUTGOING snapshot only
    /// (never `session.history` / JSONL).
    pub(crate) fn date_change_reminder_message(
        &self,
        session_id: lingxi_core::types::SessionId,
    ) -> Option<ConversationMessage> {
        let today = crate::prompt::env_meta::current_date_string();
        let session_date = self.session_start_date(session_id);
        let state = self
            .prompt_runtime
            .date_change
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if session_date == today || state.delivered_date.as_deref() == Some(today.as_str()) {
            return None;
        }
        drop(state);
        // Byte-exact reminder body (2.1.238 renderer @296739637, string-table
        // copy @256746832), wrapped by `NT`:
        // `<system-reminder>\n{e}\n</system-reminder>`. The tail sentence was
        // rewritten upstream between 2.1.220 ("DO NOT mention this to the user
        // explicitly because they are already aware.") and 2.1.238; the dash is
        // U+2014.
        let content = format!(
            "<system-reminder>\nThe date has changed. Today's date is now {today}. \
No need to announce the new date \u{2014} the user's own clock shows it.\n</system-reminder>"
        );
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// Does THIS model step continue a tool round rather than follow a fresh
    /// user prompt?
    ///
    /// The oracle's attachment fan-out (@296520120) distinguishes the two with
    /// `e === null` (no new prompt was handed to `getAttachments`) plus
    /// `!s?.isRegularUserPrompt`. LingXi's turn drivers re-enter the same
    /// assembly for both cases, so the discriminator is recovered from the
    /// history tail: a step that follows tool execution ends on a user line
    /// carrying `tool_result` blocks (claude's `sxl`, @296542062).
    pub(crate) fn step_follows_tool_results(history: &[ConversationMessage]) -> bool {
        matches!(
            history.last(),
            Some(ConversationMessage::User { content, .. })
                if content
                    .iter()
                    .any(|b| matches!(b, lingxi_core::types::ContentBlock::ToolResult { .. }))
        )
    }

    /// The per-turn, transient `silent_turn_reminder` (2.1.238, producer `K4T`
    /// @296525255), or `None` when the gate is off or the stretch is too short.
    ///
    /// Gate, 1:1 with the fan-out condition @296520120:
    /// `p && e===null && !s?.isRegularUserPrompt && !CDt() && u3m(model)` —
    /// main agent only (every `ConversationOrchestrator` is depth-0, so `p` is
    /// always true), only on a tool-round continuation, and only when the
    /// capability/env gate is on. `CDt()` is the focus/brief-transcript view
    /// mode, which LingXi does not have ⇒ always `false` ⇒ never suppresses.
    ///
    /// 2.1.263 `jfr` consults the explicit env override, then the current
    /// model's Fable 5.1 prompt bundle / silent-turn reminder capability.
    ///
    /// On fire the body is wrapped in the usual `<system-reminder>` envelope
    /// (renderer @296738727: `[kn({content:NT(e.text),isMeta:!0})]`) and the
    /// emission position is recorded so `Ezm`'s `remindersInStretch` can be
    /// reconstructed on later turns. Appended to THIS turn's OUTGOING snapshot
    /// only — never `session.history` / JSONL.
    pub(crate) async fn silent_turn_reminder_message(&self) -> Option<ConversationMessage> {
        let history = {
            let s = self.session.lock().await;
            if !crate::prompt::silent_turn::is_enabled(&s.model) {
                return None;
            }
            s.history.clone()
        };
        if !Self::step_follows_tool_results(&history) {
            return None;
        }
        let mut marks = self
            .prompt_runtime
            .silent_turn_reminder_marks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stretch = crate::prompt::silent_turn::scan_silent_stretch(&history, marks.as_slice());
        if !crate::prompt::silent_turn::should_emit(
            stretch,
            crate::prompt::silent_turn::turns_between_reminders(),
        ) {
            return None;
        }
        marks.push(history.len());
        drop(marks);
        let body = crate::prompt::silent_turn::reminder_text();
        let content = format!("<system-reminder>\n{body}\n</system-reminder>");
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// The per-turn, transient `total_tokens_reminder` (producer `D3T`
    /// @296556375), or `None` when the mode resolves to `off`.
    ///
    /// ```js
    /// let i=srt(); if(i==="off")return[];
    /// let s=n??"main", a=hoe(t), l=RYn.of(e);
    /// if(o) l.reanchorTaskBudget(s,a);
    /// let c = i==="countdown" ? OR(r,Ox())-a : i==="padded-countdown" ? uOi()-l.cumulativeUsed(s,a) : 0;
    /// return [{type:"total_tokens_reminder", text:dOi(i,c)}]
    /// ```
    ///
    /// The fan-out only calls `D3T` when the step continues a tool round
    /// (`e===null`) or when a regular user prompt arrived AND
    /// `totalTokensReminderAfterUserTurn` is on — the latter also being the
    /// `reanchor` flag. `hoe(messages)` (@294688350) is the LAST assistant
    /// message's `input + cache_creation + cache_read + output`, cached here as
    /// [`Self::last_response_input_tokens`] + [`Self::last_response_output_tokens`].
    ///
    /// **Default OFF in the port** — see the divergence note on
    /// [`crate::prompt::total_tokens`]. Stock sessions get `None`, so the
    /// locked streaming fixtures stay byte-identical.
    pub(crate) async fn total_tokens_reminder_message(&self) -> Option<ConversationMessage> {
        use crate::prompt::total_tokens as tt;
        let mode = tt::resolve_mode(None);
        if mode == tt::TotalTokensMode::Off {
            return None;
        }
        let (history_tail_is_tool_results, model) = {
            let s = self.session.lock().await;
            (Self::step_follows_tool_results(&s.history), s.model.clone())
        };
        let reanchor = !history_tail_is_tool_results && tt::after_user_turn(None);
        if !history_tail_is_tool_results && !reanchor {
            return None;
        }
        let used = i64::try_from(
            self.compaction_runtime
                .last_response_input_tokens
                .load(std::sync::atomic::Ordering::Relaxed)
                .saturating_add(
                    self.compaction_runtime
                        .last_response_output_tokens
                        .load(std::sync::atomic::Ordering::Relaxed),
                ),
        )
        .unwrap_or(i64::MAX);
        let context_window = i64::try_from(compaction::effective_context_window_size(
            &model,
            &self.api.active_betas(),
        ))
        .unwrap_or(i64::MAX);
        let budget = tt::resolve_budget(None);
        let body = {
            let mut ledger = self
                .compaction_runtime
                .total_tokens_ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if reanchor {
                ledger.reanchor_task_budget("main", used);
            }
            let remaining =
                tt::remaining_tokens(mode, &mut ledger, "main", used, context_window, budget);
            tt::format_total_tokens(mode, remaining)
        };
        // Renderer @296738663: `[kn({content:NT(e.text),isMeta:!0})]`.
        let content = format!("<system-reminder>\n{body}\n</system-reminder>");
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// The tool name that issued `tool_use_id`, recovered from the assistant
    /// line that carried the `tool_use` block.
    pub(super) async fn tool_name_for_use_id(
        &self,
        tool_use_id: &lingxi_core::types::ToolUseId,
    ) -> Option<String> {
        let s = self.session.lock().await;
        s.history.iter().rev().find_map(|msg| {
            let ConversationMessage::Assistant { content, .. } = msg else {
                return None;
            };
            content.iter().find_map(|b| match b {
                lingxi_core::types::ContentBlock::ToolUse { id, name, .. } if id == tool_use_id => {
                    Some(name.clone())
                }
                _ => None,
            })
        })
    }

    /// Return the local date memoized for `session_id`, seeding it exactly once.
    ///
    /// Both the leading `# currentDate` context and the midnight reminder use
    /// this producer, so call order cannot create two independent date memos.
    pub(super) fn session_start_date(&self, session_id: lingxi_core::types::SessionId) -> String {
        let today = crate::prompt::env_meta::current_date_string();
        let mut state = self
            .prompt_runtime
            .date_change
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.session_id != Some(session_id) {
            *state = DateChangeState {
                session_id: Some(session_id),
                session_date: today,
                delivered_date: None,
            };
        }
        state.session_date.clone()
    }

    /// Mark the current local date's `date_change` reminder as DELIVERED — the
    /// commit half of [`Self::date_change_reminder_message`]. Called once the
    /// request carrying this turn's outgoing snapshot has actually been issued.
    pub(crate) fn commit_date_change_reminder(&self) {
        self.prompt_runtime
            .date_change
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .delivered_date = Some(crate::prompt::env_meta::current_date_string());
    }

    /// Finding #73: increment BOTH reminder counters by one assistant turn.
    /// Called once per assistant turn (after the API response is processed) on
    /// both the batched and streaming paths, mirroring the binary's per-
    /// assistant-message counting in `L4p`/`N4p`.
    pub(crate) async fn bump_reminder_turn_counters(&self) {
        let mut s = self.session.lock().await;
        s.turns_since_last_todo_write = s.turns_since_last_todo_write.saturating_add(1);
        s.turns_since_last_reminder = s.turns_since_last_reminder.saturating_add(1);
    }

    /// Finding #73: reset `turns_since_last_todo_write` to `0` when this turn's
    /// assistant response invoked the variant's "recent use" tool — `TodoWrite`
    /// (V1) or `TaskCreate`/`TaskUpdate` (V2). Mirrors `L4p`/`N4p` finding the
    /// last such tool_use in the message log (which zeroes their `r` counter).
    /// `tool_names` is the set of tool names invoked in the assistant turn.
    pub(crate) async fn note_todo_reminder_tool_call(&self, tool_names: &[String]) {
        // Ungated (`select_mode_raw`): the oracle's counters advance and reset
        // regardless of whether the reminder can currently render, so a session
        // that re-enables the tools mid-flight does not inherit a stale count.
        let resets = match tool_task::reminder::select_mode_raw() {
            tool_task::reminder::ReminderMode::V1Todo => {
                tool_names.iter().any(|n| n == "TodoWrite")
            }
            tool_task::reminder::ReminderMode::V2Task => tool_names
                .iter()
                .any(|n| n == "TaskCreate" || n == "TaskUpdate"),
        };
        if resets {
            let mut s = self.session.lock().await;
            s.turns_since_last_todo_write = 0;
        }
    }

    /// The per-turn, transient `agent_listing_delta` reminder, or `None` when
    /// the gate is OFF (the default — keeps the inline-catalog build
    /// byte-identical), the `Agent` tool is absent this turn, or no NEW agent
    /// type has appeared since the last reminder. A wired DISK catalog is NOT
    /// required — built-ins are always announced (binary `aLe` uses
    /// `activeAgents`, which includes built-ins).
    ///
    /// 1:1 with claude-code's `agent_listing_delta` attachment
    /// (`getAgentListingDeltaAttachment`, attachments.ts:1490-1554 →
    /// `normalizeAttachmentForAPI`'s `'agent_listing_delta'` case,
    /// messages.ts:4194-4215):
    /// - GATE: `shouldInjectAgentListInMessages()` (env
    ///   `LINGXI_AGENT_LIST_IN_MESSAGES`, default OFF — see
    ///   [`agent::should_inject_agent_list_in_messages`]). When ON, `AgentTool`'s
    ///   description drops the inline catalog for a static pointer line and the
    ///   catalog is conveyed here instead, so the tool-schema prompt cache stops
    ///   busting on every MCP/plugin/permission-driven catalog change.
    /// - TOOL GATE: skip when the `Agent` tool is not in the registry this turn
    ///   (attachments.ts:1497-1501) — the listing would be unactionable.
    /// - ENTRIES: the merged built-ins + catalog listing via
    ///   [`agent::agent_listing_entries`] (later-wins precedence, sorted), the
    ///   same source of truth the inline prompt uses.
    /// - DELTA: emit lines only for types NOT yet announced
    ///   ([`Self::sent_agent_names`]); `is_initial` = the set was empty BEFORE
    ///   this turn (TS `announced.size === 0`). An empty delta ⇒ `None`.
    /// - RENDER: `<system-reminder>\n{header}\n{lines}\n</system-reminder>` with
    ///   the `is_initial`-conditional header (messages.ts:4197-4199), wrapped as
    ///   a meta user message.
    ///
    /// Like the skill-listing + conditional-rules reminders, the message is
    /// appended ONLY to the per-turn OUTGOING snapshot (never `session.history` /
    /// JSONL), so it is recomputed each turn and never accumulates.
    ///
    /// AGT-15 — the two branches that used to be documented as deferrals are
    /// now implemented, 1:1 with the oracle renderer @296704484:
    ///
    /// ```js
    /// if(n.length>0&&o.length>0){let a=e.isInitial?"Available agent types for the Agent tool:":"New agent types are now available for the Agent tool:";s.push(`${a}\n${n.join("\n")}`)}
    /// if(i.length>0)s.push(`The following agent types are no longer available:\n${i.map((a)=>`- ${a}`).join("\n")}`),s.push($io);
    /// if(n.length>0&&o.length>0&&e.isInitial&&e.showConcurrencyNote)s.push("When you launch multiple agents for independent work, send them in a single message with multiple tool uses so they run concurrently.");
    /// if(s.length===0)return[];
    /// return Zy([kn({content:s.join("\n\n"),isMeta:!0})])
    /// ```
    ///
    /// * REMOVAL: `removedTypes` = announced-minus-current, sorted (the oracle's
    ///   `c.sort()`, a plain lexicographic sort, NOT the `localeCompare` used for
    ///   the added list). Its section is followed by the shared ambient-context
    ///   trailer `$io` (@296730196) as a SEPARATE section, so the two are joined
    ///   by a blank line. Removed types are dropped from
    ///   [`Self::sent_agent_names`] — the oracle's `s.delete(p)` replay — so a
    ///   type that comes back is re-announced.
    ///   The Rust catalog CAN shrink mid-session: `agent_catalog` is a
    ///   `RwLock` the plugin/MCP reload path rewrites, which is exactly the
    ///   `removedTypes` case.
    /// * CONCURRENCY NOTE: gated on `isInitial && showConcurrencyNote` with
    ///   `showConcurrencyNote = Cc()!=="pro" && DZ()==="default"` (producer
    ///   @296530704) — i.e. NOT a Pro subscription and the subagent steer left at
    ///   `default`. Both signals exist in the port:
    ///   [`lingxi_core::host::subscription::is_pro_plan`] and
    ///   [`lingxi_core::host::live_sessions::subagent_steer_is_default`].
    ///
    /// NOT inert: `LINGXI_AGENT_LIST_IN_MESSAGES` defaults **ON** since 2.1.193
    /// (`lingxi_core::host::subagent_spawn::should_inject_agent_list_in_messages` returns
    /// `true` when unset), so a stock session that has the Agent tool now sends
    /// the concurrency note on its FIRST agent listing — which is exactly what
    /// 2.1.238 does for a non-Pro plan on the default steer. The removal branch
    /// stays silent until a catalog actually shrinks.
    pub(crate) async fn agent_listing_reminder_message(&self) -> Option<ConversationMessage> {
        // GATE: off by default (no GrowthBook in Rust) ⇒ no reminder, inline
        // catalog stays byte-identical.
        if !agent::should_inject_agent_list_in_messages() {
            return None;
        }
        // Gate on the Agent tool being available this turn (attachments.ts:1497).
        // Dispatch lookup also matches the legacy `Task` alias. This is the ONLY
        // structural gate in the binary's `aLe` — it does NOT gate on a wired
        // DISK catalog (see below).
        self.find_dispatchable_tool("Agent")?;

        // Merge BUILT-INS first, then the wired DISK catalog (if any) on top.
        // Built-ins are ALWAYS part of the listing — the binary's `aLe` builds
        // the delta from `activeAgents` (= built-ins + user/project agents via
        // `getAgents`), so a session with NO disk catalog still announces the
        // built-in agents. (Previously this early-returned when `agent_catalog`
        // was unset, suppressing built-ins entirely under the gate — a divergence
        // from `aLe`.) Later-wins precedence: a same-named catalog agent overrides
        // a built-in, matching the inline `AgentTool` prompt's
        // `PoolSubagentSpawner::listing_entries` (built-in < user/project).
        let mut defs = agent::builtin_agent_definitions();
        if let Some(catalog) = self.lifecycle_runtime.agent_catalog.as_ref() {
            defs.extend(catalog.read().await.iter().cloned());
        }
        let entries = agent::agent_listing_entries(&defs);

        // DELTA: keep only types not yet announced, then record them as sent;
        // and (AGT-15) compute the REMOVED set — announced types that are no
        // longer in the listing — dropping them from the announced set so a
        // type that returns is re-announced (oracle `s.delete(p)`).
        // `is_initial` is captured BEFORE inserting (TS `announced.size === 0`).
        let (is_initial, new_entries, removed_types): (
            bool,
            Vec<lingxi_core::host::subagent_spawn::SubagentListingEntry>,
            Vec<String>,
        ) = {
            let mut sent = self.prompt_runtime.sent_agent_names.lock().await;
            let is_initial = sent.is_empty();
            let current: std::collections::HashSet<String> =
                entries.iter().map(|e| e.agent_type.clone()).collect();
            let mut removed: Vec<String> = sent
                .iter()
                .filter(|t| !current.contains(t.as_str()))
                .cloned()
                .collect();
            // Oracle `c.sort()` — plain lexicographic (UTF-16 code-unit) order,
            // deliberately NOT the `localeCompare` used on the added list.
            removed.sort();
            for t in &removed {
                sent.remove(t);
            }
            let delta: Vec<_> = entries
                .into_iter()
                .filter(|e| !sent.contains(&e.agent_type))
                .collect();
            for e in &delta {
                sent.insert(e.agent_type.clone());
            }
            (is_initial, delta, removed)
        };
        if new_entries.is_empty() && removed_types.is_empty() {
            return None;
        }

        // RENDER: the oracle builds an array of SECTIONS and joins them with a
        // BLANK LINE (`s.join("\n\n")`), then wraps the whole thing in one
        // `<system-reminder>` (@296704484).
        let mut sections: Vec<String> = Vec::new();

        // 1. ADDED: header (is_initial-conditional) + one formatAgentLine per
        //    new type.
        if !new_entries.is_empty() {
            let header = if is_initial {
                "Available agent types for the Agent tool:"
            } else {
                "New agent types are now available for the Agent tool:"
            };
            // `U2n(N, D)` where `D = VU(YK(e.options.mainLoopModel))` (producer
            // @5174317): the catalog lines are rendered for the MAIN-LOOP model,
            // so a non-lean session gets a definition's full `whenToUse` even
            // when it declares a lean variant.
            let lean = tool_api::dh_simple_system_prompt(self.tools.main_loop_model().as_deref());
            let lines = new_entries
                .iter()
                .map(|entry| agent::format_agent_line(entry, lean))
                .collect::<Vec<_>>()
                .join("\n");
            sections.push(format!("{header}\n{lines}"));
        }

        // 2. REMOVED + the shared ambient-context trailer `$io` (@296730196),
        //    pushed as its OWN section so a blank line separates them.
        if !removed_types.is_empty() {
            let lines = removed_types
                .iter()
                .map(|t| format!("- {t}"))
                .collect::<Vec<_>>()
                .join("\n");
            sections.push(format!(
                "The following agent types are no longer available:\n{lines}"
            ));
            sections.push(crate::prompt::memory_update::AMBIENT_CONTEXT_TRAILER.to_string());
        }

        // 3. CONCURRENCY NOTE: initial listing only, and only when the plan is
        //    not Pro and the subagent steer is `default`
        //    (`showConcurrencyNote:Cc()!=="pro"&&DZ()==="default"`, @296530704).
        if !new_entries.is_empty()
            && is_initial
            && !lingxi_core::host::subscription::is_pro_plan()
            && lingxi_core::host::live_sessions::subagent_steer_is_default()
        {
            sections.push(
                "When you launch multiple agents for independent work, send them in a single \
message with multiple tool uses so they run concurrently."
                    .to_string(),
            );
        }

        if sections.is_empty() {
            return None;
        }
        let body = sections.join("\n\n");
        let content = format!("<system-reminder>\n{body}\n</system-reminder>");
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// REM-10 — the periodic `tool_search_usage_reminder`, or `None` (the
    /// default) when its gate is off.
    ///
    /// 1:1 with the oracle producer `Uzm` @**296553134**; see
    /// [`crate::prompt::tool_search_reminder`] for the oracle listing and for why
    /// this ships INERT (the upstream GrowthBook payload `juniper_shoal.
    /// marsh_lantern` is unset in a stock install, so `Lda()` is `null` and
    /// `Uzm` returns `[]` on its first line).
    ///
    /// Gate order, matching `Uzm`:
    /// 1. `Lda()` — [`crate::prompt::tool_search_reminder::config`]; `None` ⇒ off.
    /// 2. `if(!e||e.length===0)` — empty history ⇒ nothing.
    /// 3. `R3T` — BOTH counters must have reached `everyNTurns`.
    /// 4. `if(mBr()!=="tst")` — tool search must be in the plain enabled mode;
    ///    `tst-auto` ([`tool_api::defer::ToolSearchMode::Auto`]) is excluded.
    /// 5. `if(!bjt(t.options.tools))` — the ToolSearch tool must be present.
    /// 6. `l.length===0` — there must be at least one UNDISCOVERED deferred tool.
    /// 7. `if(c)return s("task_reminder_same_turn")` — never in the same turn as
    ///    a todo/task reminder.
    ///
    /// # Divergence (reason)
    /// `Uzm` also gates on `e1e(model)` / `!QLe(Fo(model))` — a per-model
    /// capability table and a Vertex exclusion. The port has neither table, and
    /// inventing one would gate on a guess; the remaining six gates are ported
    /// exactly.
    ///
    /// MUTATES the emission marks, so it must be called at most ONCE per
    /// outgoing model step, like every other member of this family.
    pub(crate) async fn tool_search_usage_reminder_message(
        &self,
        todo_reminder_fired_this_turn: bool,
    ) -> Option<ConversationMessage> {
        // (1) gate.
        let config = crate::prompt::tool_search_reminder::config()?;
        // (4) mode. `Enabled` is the oracle's `"tst"`; `Auto` is `"tst-auto"`.
        if self.tools.deferral().mode() != tool_api::defer::ToolSearchMode::Enabled {
            return None;
        }
        // (5) the ToolSearch tool must be available this turn.
        let tool_search = self.find_dispatchable_tool("ToolSearch")?;
        let tool_search_name = tool_search.name().to_string();

        // (2)+(3) history + both turn counters.
        let history = { self.session.lock().await.history.clone() };
        if history.is_empty() {
            return None;
        }
        let marks = self
            .prompt_runtime
            .tool_search_reminder_marks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let (since_tool_search, since_reminder) =
            crate::prompt::tool_search_reminder::count_turns(&history, &marks, &tool_search_name);
        if since_tool_search < config.every_n_turns || since_reminder < config.every_n_turns {
            return None;
        }
        // (7) never alongside a todo/task reminder.
        if todo_reminder_fired_this_turn {
            return None;
        }
        // (6) the undiscovered set — the searchable view minus what this session
        // has already loaded, sorted (oracle `.sort()`).
        let loaded: std::collections::HashSet<String> = self
            .tools
            .deferral()
            .loaded_tool_names()
            .into_iter()
            .collect();
        let mut undiscovered: Vec<String> = self
            .tools
            .tool_search_view()
            .entries()
            .into_iter()
            .map(|entry| entry.name)
            .filter(|name| !loaded.contains(name))
            .collect();
        undiscovered.sort();
        undiscovered.dedup();
        if undiscovered.is_empty() {
            return None;
        }
        let count = undiscovered.len();
        undiscovered.truncate(config.max_names);
        let body = crate::prompt::tool_search_reminder::render_reminder(
            &undiscovered,
            count,
            &tool_search_name,
        )?;
        self.prompt_runtime
            .tool_search_reminder_marks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(history.len());
        Some(ConversationMessage::user_meta(
            MessageId::new(),
            format!("<system-reminder>\n{body}\n</system-reminder>"),
        ))
    }

    /// REM-05 — the per-turn `edited_text_file` (changed-files) reminders: one
    /// meta user message per file that changed ON DISK since the model last saw
    /// it.
    ///
    /// 1:1 with the oracle producer `Izm(ctx)` @**296537358**:
    ///
    /// ```js
    /// async function Izm(e){let t=OWr(e.readFileState);if(t.length===0)return[];
    ///  let r=gn(e),o=(await Promise.all(t.map(async(s)=>{
    ///    let a=e.readFileState.get(s);if(!a)return null;
    ///    if(a.offset!==void 0||a.limit!==void 0)return null;
    ///    let l=Zi(s);if(qhe(l,r))return null;
    ///    try{ if(await f4e(l)<=a.timestamp)return null;
    ///         …let p=await mC.call({file_path:l},e);
    ///         if(p.data.type==="text"){ if(p.data.file.truncatedByTokenCap===!0)return null;
    ///           if(vNe(a,p.data.file.content))return null;
    ///           let f=SEf(a.content,p.data.file.content); if(f==="")return null;
    ///           return{type:"edited_text_file",filename:l,snippet:f}} …}
    ///    catch(c){if(ur(c))e.readFileState.delete(s);return null}})))
    ///   .filter((s)=>s!=null), i=0;
    ///  for(let s of o){…if(i>=m3T)s.snippet="";else i+=s.snippet.length}
    ///  return o}
    /// ```
    ///
    /// Step for step:
    ///
    /// 1. **Scan** every MODEL-VISIBLE read-state entry, in the LRU's MRU→LRU
    ///    order, through
    ///    [`tool_api::read_file_state::ReadFileStateLru::peek`] so the scan does
    ///    not rewrite recency (the oracle iterates the Map, which does not
    ///    either).
    ///
    ///    # Divergence (reason)
    ///    `OWr(e.readFileState)` yields EVERY key. LingXi additionally carries
    ///    HOST-SEEDED snapshots in the same registry, explicitly flagged
    ///    `in_model_context: false` ("the host marks this content as not present
    ///    in the model context", `seed_read_state_from_host`). Telling the model
    ///    a file "changed on disk since you last read it" when it never read it
    ///    would be a lie, so the scan uses `model_context_keys()`. In claude-code
    ///    every `readFileState` entry IS model context, so the two sets coincide
    ///    there.
    /// 2. **Skip partial reads** — `a.offset!==void 0||a.limit!==void 0`.
    ///    Also skip `seeded_from_context` / `is_partial_view` entries: their
    ///    recorded content is deliberately NOT the on-disk bytes (frontmatter
    ///    stripping, token-cap truncation), so diffing them against disk would
    ///    emit a bogus reminder every turn. That is this port's stand-in for the
    ///    oracle's `truncatedByTokenCap===!0` early return.
    /// 3. **mtime gate** — `if(await f4e(l)<=a.timestamp)return null`. A missing
    ///    file DROPS the entry (`if(ur(c))e.readFileState.delete(s)`).
    /// 4. **Re-read + content compare** — `vNe(a,content)`: identical bytes ⇒ no
    ///    reminder (but the entry's timestamp is refreshed so the mtime gate
    ///    stops firing).
    /// 5. **Diff** — [`crate::prompt::changed_files::render_snippet`] (`SEf`,
    ///    `structuredPatch` at context 8, 8192-char cap); an empty diff ⇒ no
    ///    reminder.
    /// 6. **Budget** — [`crate::prompt::changed_files::apply_snippet_budget`]
    ///    (`m3T = 16384`, cumulative across the turn's files; entries past the
    ///    threshold render the "diff is omitted here" arm).
    /// 7. **Render** — [`crate::prompt::changed_files::render_changed_file`],
    ///    each wrapped in its own `<system-reminder>` and marked meta, matching
    ///    `Zy([kn({content:…,isMeta:!0})])` per attachment.
    ///
    /// The re-read REWRITES the read-state entry (content + mtime), which is
    /// what the oracle's nested `Read` tool call does as a side effect — it is
    /// what stops the reminder from repeating every turn for the same edit.
    ///
    /// This is LIVE: there is no gate. It is silent in the common case because
    /// nothing fires unless a tracked file's mtime actually moved outside the
    /// session's own Read/Write path.
    pub(crate) async fn changed_files_reminder_messages(&self) -> Vec<ConversationMessage> {
        // (1) Snapshot the registry without touching recency.
        let candidates: Vec<(std::path::PathBuf, tool_api::read_file_state::ReadFileEntry)> = {
            let guard = self
                .prompt_runtime
                .read_state_map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .model_context_keys()
                .into_iter()
                .filter_map(|p| guard.peek(&p).map(|e| (p, e)))
                .collect()
        };
        if candidates.is_empty() {
            return Vec::new();
        }

        let read_tool_name = self
            .tools
            .find_by_name("Read")
            .map_or_else(|| "Read".to_string(), |t| t.name().to_string());

        let mut changed: Vec<crate::prompt::changed_files::ChangedFile> = Vec::new();
        for (path, entry) in candidates {
            // (2) partial / not-disk-faithful entries never participate.
            if entry.offset.is_some()
                || entry.limit.is_some()
                || entry.seeded_from_context
                || entry.is_partial_view
            {
                continue;
            }
            // (3) mtime gate; a vanished file drops its entry.
            let mtime_ms = match tokio::fs::metadata(&path).await.and_then(|m| m.modified()) {
                Ok(t) => tool_api::read_file_state::mtime_ms_floor(t),
                Err(err) => {
                    if err.kind() == std::io::ErrorKind::NotFound {
                        if let Ok(mut guard) = self.prompt_runtime.read_state_map.lock() {
                            let _ = guard.remove(&path);
                        }
                    }
                    continue;
                }
            };
            if mtime_ms <= entry.mtime_ms {
                continue;
            }
            // (4) re-read. A non-UTF-8 / unreadable file is skipped entirely —
            // the oracle's `mC.call` would have returned an image/pdf/notebook
            // payload, none of which produce an `edited_text_file`.
            let Ok(fresh) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            // Refresh the entry either way, so the mtime gate does not re-fire
            // on the next turn for the same on-disk state (the side effect the
            // oracle gets from re-reading through the Read tool).
            let unchanged = fresh == entry.content;
            tool_api::read_file_state::set_with_model_context(
                &self.prompt_runtime.read_state_map,
                path.clone(),
                tool_api::read_file_state::ReadFileEntry {
                    content: fresh.clone(),
                    mtime_ms,
                    ..entry.clone()
                },
                true,
            );
            if unchanged {
                continue;
            }
            // (5) diff.
            let snippet =
                crate::prompt::changed_files::render_snippet(&entry.content, &fresh, false);
            if snippet.is_empty() {
                continue;
            }
            changed.push(crate::prompt::changed_files::ChangedFile {
                filename: path.to_string_lossy().into_owned(),
                snippet,
            });
        }
        if changed.is_empty() {
            return Vec::new();
        }
        // (6) cross-file snippet budget, then (7) render one wrapped meta
        // message per changed file.
        crate::prompt::changed_files::apply_snippet_budget(&mut changed);
        changed
            .iter()
            .map(|file| {
                let body = crate::prompt::changed_files::render_changed_file(file, &read_tool_name);
                ConversationMessage::user_meta(
                    MessageId::new(),
                    format!("<system-reminder>\n{body}\n</system-reminder>"),
                )
            })
            .collect()
    }

    /// §F: the per-turn, transient `conditional_rules` reminder — path-gated
    /// LINGXI.md rules (`paths:`-globbed) that newly ACTIVATE because a file the
    /// session has touched this run matches their globs. Returns `None` when no
    /// memory provider is wired, the hierarchy has no conditional rules, or no
    /// newly-activated rule exists this turn.
    ///
    /// 1:1 with claude-code `processConditionedMdRules` (claudemd.ts:1354-1397)
    /// fed through the `nested_memory` render seam (messages.ts:3700-3707):
    ///
    /// 1. CACHE: the first call loads the full hierarchy (`memory.load(&cwd)` —
    ///    the same call the system prompt uses) and caches the `globs.is_some()`
    ///    subset in [`Self::conditional_rules_cache`]. Later turns reuse the cache
    ///    — no disk re-walk — and only re-test it against the latest touched set.
    /// 2. MATCH: for each cached rule and each touched file in
    ///    [`Self::read_state_map`] (the tools' live-cwd absolutized paths),
    ///    [`crate::prompt::conditional_rules::rule_matches_touched_file`] derives
    ///    the rule's base dir (Project → parent-of-`.claude`; else `cwd`),
    ///    relativizes + guards the touched path, and gitignore-tests it against
    ///    the rule's globs. A rule with ANY matching touched file is ACTIVE.
    /// 3. DELTA: a rule already in [`Self::sent_conditional_rules`] is skipped
    ///    (TS `loadedNestedMemoryPaths`), so each rule injects ONCE. Newly-active
    ///    rules are recorded as sent and rendered.
    /// 4. RENDER: each newly-active rule becomes a bare `Contents of {path}:` body
    ///    wrapped in `<system-reminder>` (messages.ts `nested_memory`), joined by
    ///    a blank line into one meta user message (TS pushes one wrapped message
    ///    per rule; concatenation here is byte-equivalent for a single rule and a
    ///    faithful grouping for several).
    ///
    /// Appended ONLY to the per-turn outgoing snapshot (never `session.history` /
    /// JSONL), exactly like the skill-listing + output-style reminders.
    /// Per-turn, transient `<new-diagnostics>` reminder — newly-reported LSP
    /// diagnostics not yet surfaced to the model (claude-code's
    /// `formatDiagnosticsBlock` flow). `None` when no LSP source is wired (no
    /// servers ⇒ the common case) or there are no new diagnostics.
    ///
    /// The block carries its own `<new-diagnostics>` tag, and the oracle wraps
    /// that in a `<system-reminder>` on top of it: 2.1.238 @296692400
    /// `case"diagnostics":{…return Zy([kn({content:Bve.formatDiagnosticsBlock(n),isMeta:!0})])}`,
    /// where `Zy` (@296675470) maps `NT` = `` `<system-reminder>\n${e}\n</system-reminder>` ``
    /// (@296673554) over every message. The `<new-diagnostics>` literal
    /// (@236015184) contains no envelope of its own, so the two tags nest.
    /// Appended ONLY to the outgoing snapshot (never `session.history` / JSONL).
    pub(crate) async fn new_diagnostics_reminder_message(&self) -> Option<ConversationMessage> {
        let block = self
            .prompt_runtime
            .new_diagnostics_source
            .as_ref()?
            .take_new_diagnostics_block()
            .await?;
        let content = format!("<system-reminder>\n{block}\n</system-reminder>");
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    pub(crate) async fn conditional_rules_reminder_message(&self) -> Option<ConversationMessage> {
        // (1) CACHE — fill once from the same memory load the system prompt
        // uses. Task 5 (worktree 206 session-cwd plumbing): `cwd` is the LIVE
        // `self.session_cwd` (not the frozen `self.cwd`), and
        // `conditional_rules_cache` is reset to `None` by the `set_on_swap`
        // callback [`Self::with_session_cwd`] registers, so a worktree swap
        // forces this to re-walk disk under the NEW cwd instead of replaying
        // the pre-swap directory's rule set for the rest of the session.
        let cwd = self.session_cwd.cwd();
        let cached: Option<Vec<crate::prompt::MemoryFile>> = self
            .prompt_runtime
            .conditional_rules_cache
            .lock()
            .unwrap()
            .clone();
        let rules: Vec<crate::prompt::MemoryFile> = match cached {
            Some(rules) => rules,
            None => {
                let loaded = self
                    .memory
                    .load(&cwd)
                    .await
                    .into_iter()
                    .filter(|f| f.globs.is_some())
                    .collect::<Vec<_>>();
                *self.prompt_runtime.conditional_rules_cache.lock().unwrap() = Some(loaded.clone());
                loaded
            }
        };
        if rules.is_empty() {
            return None;
        }

        // Snapshot the touched files from the shared read-state registry (the
        // tools' live-cwd absolutized Read/Edit/Write/… paths).
        let touched: Vec<std::path::PathBuf> = self
            .prompt_runtime
            .read_state_map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .model_context_keys();
        if touched.is_empty() {
            return None;
        }

        // (2)+(3) MATCH + DELTA — collect newly-active rules not yet sent.
        let mut newly_active: Vec<&crate::prompt::MemoryFile> = Vec::new();
        {
            let mut sent = self.prompt_runtime.sent_conditional_rules.lock().await;
            for rule in &rules {
                if sent.contains(&rule.path) {
                    continue; // already injected this session
                }
                let active = touched.iter().any(|t| {
                    crate::prompt::conditional_rules::rule_matches_touched_file(rule, t, &cwd)
                });
                if active {
                    sent.insert(rule.path.clone());
                    newly_active.push(rule);
                }
            }
        }
        if newly_active.is_empty() {
            return None;
        }

        // (4) RENDER — one `<system-reminder>` block per rule, joined by a blank
        // line into a single meta user message.
        let content = newly_active
            .iter()
            .map(|r| crate::prompt::conditional_rules::render_reminder(r))
            .collect::<Vec<_>>()
            .join("\n\n");
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// The per-turn NESTED MEMORY reminder: the `LINGXI.md` (and matching
    /// `paths:`-gated rules) governing the directories of files the session has
    /// TOUCHED. Guidance that lives next to the code reaches the model when the
    /// model reaches the code.
    ///
    /// 1:1 with claude-code `k$o` (@237714543) driven by `Rop` (@237715260):
    ///
    /// ```js
    /// for(let i of e){
    ///   if(t.loadedNestedMemoryPaths?.[i.path])continue;
    ///   if(!t.readFileState.has(i.path)){ n.push({type:"nested_memory",…});
    ///     t.loadedNestedMemoryPaths[i.path]=!0;
    ///     t.readFileState.set(i.path,{…,seededFromContext:!0,keepContent:!0}) }}
    /// ```
    ///
    /// 1. DISCOVER — [`crate::prompt::nested_memory::discover`] per touched
    ///    file. Stateless by design; see [`Self::sent_nested_memory`].
    /// 2. SKIP — anything already sent (`loadedNestedMemoryPaths`), already
    ///    claimed by [`Self::conditional_rules_reminder_message`], or already in
    ///    `read_file_state` (the model has the real thing).
    /// 3. SEED — [`Self::seed_nested_memory_read_state`], so the next `Read` of
    ///    a surfaced file returns the dedup stub instead of the bytes again.
    /// 4. RENDER — [`crate::prompt::conditional_rules::render_reminder`], the
    ///    same bare `Contents of {path}:` shape the oracle's `nested_memory`
    ///    attachment renders to.
    ///
    /// MUTATES the sent-set, so it must be called at most ONCE per outgoing
    /// model step — the same constraint every reminder in this family carries.
    ///
    /// # Divergence (reason)
    /// `Rop` opens with `if(!zK(e,r.toolPermissionContext))return n` — a
    /// read-permission check on the TRIGGER file. LingXi's permission context
    /// is not plumbed to this layer, and the trigger is by construction a file
    /// a tool already read, so the gate would be a no-op here. Not invented.
    ///
    /// `pub` (unlike its `pub(crate)` siblings) only so `test-harness` can drive
    /// it against a real `FileReadTool`: the end-to-end seed-then-dedup proof
    /// needs the real tool's registration + permission plumbing, which does not
    /// exist in this crate's unit tests. (`orchestrator` now carries a `tool-file`
    /// dependency for REM-05's `structuredPatch`, but only the diff function —
    /// not the tool.) Both turn drivers are still the only production callers.
    pub async fn nested_memory_reminder_message(&self) -> Option<ConversationMessage> {
        // Same env kill-switch the eager loader honors (`Rop`'s
        // `CLAUDE_CODE_DISABLE_CLAUDE_MDS` guard). ANY non-empty value disables.
        if std::env::var_os("LINGXI_DISABLE_LINGXI_MDS").is_some_and(|v| !v.is_empty()) {
            return None;
        }
        let touched: Vec<std::path::PathBuf> = self
            .prompt_runtime
            .read_state_map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .model_context_keys();
        if touched.is_empty() {
            return None;
        }
        let (home, managed) = match &self.prompt_runtime.nested_memory_roots {
            Some((home, managed)) => (home.clone(), managed.clone()),
            None => (
                dirs::home_dir()?,
                Some(memory::lingxi_md::hierarchy::managed_path()),
            ),
        };
        // CANONICAL cwd, not the raw one. `split_ancestors` decides "is the
        // touched file under cwd" with a prefix test, and the two sides reach
        // it in different forms: `read_state_map` keys are whatever
        // `canonicalize_and_validate` produced, while `session_cwd` is whatever
        // the user launched in. On macOS that is `/private/var/...` against
        // `/var/...`, so every touched file looks OUTSIDE cwd and nothing is
        // ever discovered. The oracle has no such split (its `Li` is purely
        // lexical, so both halves agree); LingXi has to normalize on ONE side,
        // and cwd is the side that makes every derived path match the registry
        // the seed writes to. Falls back to the raw cwd if it does not exist.
        let cwd = tokio::fs::canonicalize(self.session_cwd.cwd())
            .await
            .unwrap_or_else(|_| self.session_cwd.cwd());
        let excluder = self.memory.excluder();

        let mut surfaced: Vec<crate::prompt::MemoryFile> = Vec::new();
        {
            let mut sent = self.prompt_runtime.sent_nested_memory.lock().await;
            let mut sent_rules = self.prompt_runtime.sent_conditional_rules.lock().await;
            for trigger in &touched {
                for f in crate::prompt::nested_memory::discover_with_excludes(
                    trigger,
                    &cwd,
                    &home,
                    managed.as_deref(),
                    excluder.as_ref(),
                ) {
                    if sent.contains(&f.path) {
                        continue;
                    }
                    // A `paths:`-gated rule is owned by BOTH mechanisms; the
                    // shared set means whichever reaches the model first wins
                    // and the other stands down. Unconditional memory files
                    // never enter this set — conditional rules is not their
                    // owner and marking them would be a lie.
                    if f.globs.is_some() && sent_rules.contains(&f.path) {
                        continue;
                    }
                    // `!t.readFileState.has(i.path)`, canonical-keyed like the
                    // registry itself. Note the oracle does NOT mark such a
                    // path as loaded — it stays in `readFileState` forever, so
                    // it stays skipped either way.
                    let key = tokio::fs::canonicalize(&f.path)
                        .await
                        .unwrap_or_else(|_| f.path.clone());
                    if self
                        .prompt_runtime
                        .read_state_map
                        .lock()
                        .is_ok_and(|guard| guard.contains(&key))
                    {
                        continue;
                    }
                    sent.insert(f.path.clone());
                    if f.globs.is_some() {
                        sent_rules.insert(f.path.clone());
                    }
                    surfaced.push(f);
                }
            }
        }
        if surfaced.is_empty() {
            return None;
        }
        // The oracle's `k$o` returns RECORDS, not text — the reminder is one
        // rendering of them and the UI attachment line is the other. The port
        // originally took only the text half, so the attachment cells the TUI
        // already knows how to draw had no producer. `displayPath` is the
        // oracle's `relative(cwd, path)`.
        for file in &surfaced {
            let display_path = file
                .path
                .strip_prefix(&cwd)
                .unwrap_or(&file.path)
                .display()
                .to_string();
            self.output
                .emit_attachment(lingxi_core::host::AttachmentKind::NestedMemory { display_path })
                .await;
        }
        self.seed_nested_memory_read_state(&surfaced).await;
        let content = surfaced
            .iter()
            .map(crate::prompt::conditional_rules::render_reminder)
            .collect::<Vec<_>>()
            .join("\n\n");
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// P0.1: arm the memory-selector prefetch for THIS turn, firing it
    /// CONCURRENTLY with the main API call (claude-code's `wAo` prefetch
    /// side-channel). Called at the START of each turn in BOTH drivers, BEFORE
    /// the snapshot is assembled, so the in-flight handle is ready for
    /// [`Self::relevant_memory_reminder_messages`] to await. A strict no-op when
    /// no prefetch is wired ([`Self::memory_prefetch`] is `None`) — then the slot
    /// stays empty and the surfacing reminder list is empty, keeping the locked
    /// fixtures byte-identical.
    ///
    /// The prefetch query is the latest NON-meta user-message text in the
    /// session history (mirroring TS `e.findLast(m => m.type==="user" &&
    /// !m.isMeta)` in `wAo`). The memdir directory is derived from the cwd; the
    /// stub prefetch ignores both for now (it resolves to an empty set) so this
    /// is inert by default.
    pub(super) async fn discard_stale_prefetches(&self) {
        *self.prompt_runtime.pending_memory_prefetch.lock().await = None;
        *self.prompt_runtime.pending_skill_prefetch.lock().await = None;
    }

    fn has_visible_prefetch_text(text: &str) -> bool {
        text.chars().any(|ch| {
            !ch.is_whitespace()
                && !matches!(
                    ch,
                    '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}'
                )
        })
    }

    fn latest_prefetch_query(history: &[ConversationMessage]) -> String {
        history
            .iter()
            .rev()
            .find_map(|message| match message {
                ConversationMessage::User {
                    content,
                    is_meta: false,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                    ..
                } => {
                    let text = content
                        .iter()
                        .filter_map(|block| match block {
                            lingxi_core::types::ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    Self::has_visible_prefetch_text(&text).then_some(text)
                }
                _ => None,
            })
            .unwrap_or_default()
    }

    fn latest_prefetch_query_and_tools(history: &[ConversationMessage]) -> (String, Vec<String>) {
        let query = Self::latest_prefetch_query(history);
        let last_assistant_tools = history
            .iter()
            .rev()
            .find(|m| matches!(m.role(), lingxi_core::types::MessageRole::Assistant))
            .map(|m| {
                m.tool_calls()
                    .into_iter()
                    .filter_map(|b| match b {
                        lingxi_core::types::ContentBlock::ToolUse { name, .. } => {
                            Some(name.clone())
                        }
                        _ => None,
                    })
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        (query, last_assistant_tools)
    }

    pub(crate) async fn start_memory_prefetch(&self) {
        let Some(prefetch) = self.prompt_runtime.memory_prefetch.as_ref() else {
            return; // no prefetch wired ⇒ surfacing channel stays inert
        };
        // Keep at most one in-flight selector. A slow result continues under
        // the current model call instead of being overwritten or queueing
        // unbounded side queries.
        if self
            .prompt_runtime
            .pending_memory_prefetch
            .lock()
            .await
            .is_some()
        {
            return;
        }
        // Latest REAL user message = the turn query. Compact summaries,
        // transcript-only rows, Stop-hook feedback, and tool-result user rows
        // are synthetic context rather than user intent.
        let (query, session_id) = {
            let s = self.session.lock().await;
            // Memory selection must follow the same model-visible history as
            // request assembly. Transcript-only/background rows can remain in
            // the UI history, but they are not a valid user query and must not
            // move the selector's starting point.
            let history = s.model_context_history();
            (
                Self::latest_prefetch_query(&history),
                s.session_id.to_string(),
            )
        };
        // Task 5 (worktree 206 session-cwd plumbing): the live cwd, so a future
        // non-stub prefetch derives the memdir from the post-swap worktree, not
        // the frozen boot cwd. Currently inert (the stub prefetch ignores its
        // cwd argument), so this is a no-behavior-change correctness fix.
        let pending = prefetch
            .start_for_session(query, self.session_cwd.cwd(), Some(session_id))
            .await;
        *self.prompt_runtime.pending_memory_prefetch.lock().await = Some(pending);
    }

    /// P0.1: the per-turn, transient `relevant_memories` SURFACING reminders — the
    /// memory-selector/prefetch result rendered as one meta user message per
    /// surfaced memory. Returns an empty list when no prefetch was armed this turn
    /// ([`Self::start_memory_prefetch`] left the slot empty / no prefetch wired),
    /// the prefetch resolved to an empty set, or every surfaced memory was
    /// already injected (the SHARED dedup below).
    ///
    /// 1:1 with claude-code v2.1.181's `relevant_memories` attachment
    /// (`normalizeAttachmentForAPI` case `"relevant_memories"`, messages.ts —
    /// see [`memory::surfacing::render_surfacing_messages`] for the exact shape):
    /// the em-dash idx-0 preamble + per-memory `Memory: {path}:` header (with a
    /// `>1`-day staleness prefix), preserving each memory's message boundary.
    ///
    /// SHARED DEDUP: a memory is skipped when its path is in EITHER
    /// [`Self::surfaced_memory_paths`] (already surfaced a prior turn) OR
    /// [`Self::read_state_map`] (already loaded as a nested/conditional P3.2
    /// attachment OR read by a file tool) — so a file can never be double-injected
    /// across the surfacing + nested channels. Surfaced paths are recorded so each
    /// memory injects ONCE (TS prefetch consume-once + `loadedNestedMemoryPaths`).
    ///
    /// Like every other per-turn reminder, the message is appended ONLY to the
    /// per-turn OUTGOING snapshot (never `session.history` / JSONL), so it is
    /// recomputed each turn and never accumulates.
    pub(crate) async fn relevant_memory_reminder_messages(&self) -> Vec<ConversationMessage> {
        // Never await an unresolved side query on the model-call critical path.
        // Leave it in the slot so it can run concurrently with this iteration
        // and be collected by a later one.
        let pending = {
            let mut slot = self.prompt_runtime.pending_memory_prefetch.lock().await;
            let Some(pending) = slot.take() else {
                return Vec::new();
            };
            if !pending.is_ready() {
                *slot = Some(pending);
                return Vec::new();
            }
            pending
        };
        let surfaced = pending.take().await;
        if surfaced.is_empty() {
            return Vec::new();
        }

        // SHARED DEDUP — skip any memory already surfaced this session OR already
        // loaded as a nested/conditional attachment / tool read (`read_state_map`).
        let already_read: std::collections::HashSet<std::path::PathBuf> = self
            .prompt_runtime
            .read_state_map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .model_context_keys()
            .into_iter()
            .collect();
        let fresh: Vec<memory::surfacing::SurfacedMemory> = {
            let mut surfaced_set = self.prompt_runtime.surfaced_memory_paths.lock().await;
            let mut out = Vec::new();
            for m in surfaced {
                if surfaced_set.contains(&m.path) || already_read.contains(&m.path) {
                    continue; // double-injection guard
                }
                surfaced_set.insert(m.path.clone());
                out.push(m);
            }
            out
        };
        if fresh.is_empty() {
            return Vec::new();
        }

        memory::surfacing::render_surfacing_messages(&fresh)
            .into_iter()
            .map(|content| ConversationMessage::user_meta(MessageId::new(), content))
            .collect()
    }

    /// EXPERIMENTAL_SKILL_SEARCH: arm the skill-discovery prefetch CONCURRENTLY
    /// with this turn (1:1 with claude-code `startSkillDiscoveryPrefetch`, bundle
    /// fn `C1z`: `B=at1?.startSkillDiscoveryPrefetch(null,V,T)` at iteration top).
    /// A strict no-op when no prefetch is wired ([`Self::skill_discovery_prefetch`]
    /// is `None`) — then the slot stays empty and the surfacing reminder is `None`,
    /// keeping the locked fixtures byte-identical.
    ///
    /// The prefetch query is the latest non-meta user-message text (same scan as
    /// [`Self::start_memory_prefetch`], mirroring TS `findLast(user/!meta)`). The
    /// per-iteration `findWritePivot` guard (`query.ts:323` — discovery only fires
    /// on write-pivot iterations) is computed from the most recent assistant
    /// message's requested tools (see [`skill_api::find_write_pivot`],
    /// [RECONSTRUCTED]); on a non-write iteration the prefetch ships empty.
    pub(crate) async fn start_skill_discovery_prefetch(&self) {
        let Some(prefetch) = self.prompt_runtime.skill_discovery_prefetch.as_ref() else {
            return; // no prefetch wired ⇒ discovery channel stays inert
        };
        // One bounded in-flight discovery. A slow result remains eligible for a
        // later iteration and never queues another side query behind it.
        if self
            .prompt_runtime
            .pending_skill_prefetch
            .lock()
            .await
            .is_some()
        {
            return;
        }
        // Latest non-meta user message = the turn query (TS findLast user/!meta),
        // and the most recent assistant message's tool names for the write-pivot
        // predicate — both read in one history lock.
        let (query, last_assistant_tools) = {
            let s = self.session.lock().await;
            // Keep the query and write-pivot scan on the exact same
            // model-visible slice. UI-only rows are deliberately excluded.
            let history = s.model_context_history();
            Self::latest_prefetch_query_and_tools(&history)
        };
        let is_write_pivot = skill_api::find_write_pivot(&last_assistant_tools);
        let pending = prefetch.start(query, is_write_pivot).await;
        *self.prompt_runtime.pending_skill_prefetch.lock().await = Some(pending);
    }

    /// EXPERIMENTAL_SKILL_SEARCH: the per-turn, transient `skill_discovery`
    /// SURFACING reminder — the prefetch result rendered as a single
    /// `<system-reminder>` meta user message (1:1 with claude-code's
    /// `collectSkillDiscoveryPrefetch` → `skill_discovery` attachment,
    /// `messages.ts:3506-3519`). Returns `None` when no prefetch was armed this
    /// turn, the prefetch resolved to an empty set, or every discovered skill was
    /// already surfaced.
    ///
    /// Emits the `hidden_by_main_turn` telemetry field (`query.ts:1617`): `true`
    /// when the prefetch resolved BEFORE collection (it hid under the main turn's
    /// streaming + tool execution; expected >98%). Peeked via
    /// [`skill_api::PendingSkillDiscoveryPrefetch::is_ready`] before the consuming
    /// `take`.
    ///
    /// DEDUP: a skill is skipped when its `name` is in
    /// [`Self::surfaced_skill_names`] (already surfaced a prior turn). Keyed on
    /// `name` (skill names are not files, so — unlike the memory channel — this
    /// does NOT consult `read_file_state`). Surfaced names are recorded so each
    /// skill injects ONCE. Like every other per-turn reminder, the message is
    /// appended ONLY to the per-turn OUTGOING snapshot (never `session.history` /
    /// JSONL).
    pub(crate) async fn skill_discovery_reminder_message(&self) -> Option<ConversationMessage> {
        // Consume the in-flight prefetch handle armed at turn start.
        let pending = {
            let mut slot = self.prompt_runtime.pending_skill_prefetch.lock().await;
            let pending = slot.take()?;
            // A side query that has not hidden under available work must not
            // delay the API call. Keep it alive and try again next iteration.
            if !pending.is_ready() {
                *slot = Some(pending);
                telemetry::emit_skill_discovery_collected(false);
                return None;
            }
            pending
        };
        telemetry::emit_skill_discovery_collected(true);

        let skills = pending.take().await;
        if skills.is_empty() {
            return None;
        }

        // DEDUP by name — skip any skill already surfaced this session.
        let fresh: Vec<skill_api::DiscoveredSkill> = {
            let mut surfaced = self.prompt_runtime.surfaced_skill_names.lock().await;
            let mut out = Vec::new();
            for s in skills {
                if surfaced.contains(&s.name) {
                    continue; // already surfaced a prior turn
                }
                surfaced.insert(s.name.clone());
                out.push(s);
            }
            out
        };

        // render returns None on empty (TS `return []`).
        let content = skill_api::render_skill_discovery_block(&fresh)?;
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// `deferred_tools_delta` reminder for THIS outgoing model step, prepended to
    /// the transient snapshot and never persisted.
    ///
    /// Claude Code (`A1s` + the `deferred_tools_delta` attachment renderer)
    /// announces the searchable deferred set as a `<system-reminder>` only when
    /// that set has CHANGED since the prior request — listing the newly-available
    /// names, re-appearing names (MCP reconnect), and removed names — rather than
    /// repeating the whole catalog every turn. The announced / ever-added state
    /// is tracked in the session's [`DeferralState`].
    ///
    /// MUTATES the delta tracking (via `compute_deferred_delta`), so it must be
    /// called at most ONCE per outgoing model step. Callers that rebuild the
    /// snapshot for a retry of the SAME step (streaming recovery / non-streaming
    /// fallback) reuse the value computed here instead of re-invoking it.
    pub(crate) fn deferred_tools_reminder_message(&self) -> Option<ConversationMessage> {
        if !self.tools.deferral().is_enabled() {
            return None;
        }
        // Currently-deferred (undiscovered) set = oracle `g`: the searchable
        // view minus tools already loaded this session. The view is the
        // `wants_defer` candidate set (which still includes loaded tools), so
        // subtract the loaded names to obtain the `should_defer` set.
        let loaded: std::collections::HashSet<String> = self
            .tools
            .deferral()
            .loaded_tool_names()
            .into_iter()
            .collect();
        let mut current: Vec<String> = self
            .tools
            .tool_search_view()
            .entries()
            .into_iter()
            .map(|entry| entry.name)
            .filter(|name| !loaded.contains(name))
            .collect();
        current.sort();
        current.dedup();
        let delta = self.tools.deferral().compute_deferred_delta(&current);
        let body = delta.render_reminder()?;
        Some(ConversationMessage::user_meta(MessageId::new(), body))
    }
}

#[cfg(test)]
mod prefetch_history_tests {
    use super::*;
    use lingxi_core::types::{ConversationMessage, MessageId, SessionId};
    use lingxi_core::SessionState;

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.to_string())
    }

    #[test]
    fn prefetch_starts_from_model_visible_history() {
        let excluded = user("background transcript row");
        let invisible = user("\u{200B}\u{FEFF}");
        let current = user("current request");
        let mut state = SessionState::empty(SessionId::nil(), "model".into());
        state.model_context_excluded_messages.insert(excluded.id());
        state.history.extend([excluded, invisible, current]);

        let visible = state.model_context_history();
        let (query, tools) = ConversationOrchestrator::latest_prefetch_query_and_tools(&visible);

        assert_eq!(query, "current request");
        assert!(tools.is_empty());
    }
}
