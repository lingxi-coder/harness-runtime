//! Compaction orchestrator — runs each layer in order, escalating only when
//! cheaper layers leave us over the autocompact threshold.
//!
//! Order mirrors the TS query pipeline (`query.ts:400-467`): **snip →
//! microcompact → context-collapse projection → autocompact**. Context collapse
//! is session-owned and default-off; the conversation layer replays its
//! committed projection on every outgoing request while this orchestrator owns
//! the store and the prompt-too-long drain primitive.
//!
//! Autocompact is gated through [`crate::threshold_calc::should_auto_compact`]
//! and guarded by the circuit breaker from `autoCompactIfNeeded`
//! (`autoCompact.ts:241-351`): after
//! [`MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES`](crate::thresholds::MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES)
//! consecutive failures the layer short-circuits without calling the
//! summarizer.

use crate::autocompact::{Autocompactor, CompactionError};
use crate::cached_microcompact::CachedMicrocompact;
use crate::context_collapse::ContextCollapse;
use crate::microcompact::{Microcompactor, TimeBasedMCConfig};
use crate::snip::SnipCompactor;
use crate::thresholds::{
    rapid_refill_count, AutoCompactTrackingState, CompactionLayer,
    MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES, MAX_CONSECUTIVE_RAPID_REFILLS,
};
use cost::Usage;
use lingxi_core::types::{ContentBlock, ConversationMessage};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

fn media_analysis_messages(messages: &[ConversationMessage]) -> Vec<ConversationMessage> {
    messages
        .iter()
        .filter_map(|message| match message {
            ConversationMessage::User {
                id,
                content,
                is_meta,
                is_compact_summary,
                is_visible_in_transcript_only,
             .. } => {
                let analysis_blocks = content
                    .iter()
                    .filter(|block| matches!(block, ContentBlock::MediaAnalysis { .. }))
                    .cloned()
                    .collect::<Vec<_>>();
                (!analysis_blocks.is_empty()).then(|| ConversationMessage::User { api_message_override: None,
                    id: *id,
                    content: analysis_blocks,
                    is_meta: *is_meta,
                    is_compact_summary: *is_compact_summary,
                    is_visible_in_transcript_only: *is_visible_in_transcript_only,
                })
            }
            ConversationMessage::Assistant { .. } | ConversationMessage::System { .. } => None,
        })
        .collect()
}

/// Result of one orchestrator pass.
#[derive(Debug, Clone)]
pub struct IterationCompactionResult {
    /// Compacted message list.
    pub messages: Vec<ConversationMessage>,
    /// Trimmed original model summary for the `PostCompact` hook payload.
    pub raw_summary_text: String,
    /// Layers that actually fired this iteration, in order.
    pub layers_applied: Vec<CompactionLayer>,
    /// Approximate tokens freed across all layers.
    pub total_tokens_freed: u64,
    /// Whether the microcompact result came from the same-input cache.
    pub cache_hit: bool,
    /// Consecutive autocompact-failure count after this pass. Mirrors the
    /// `consecutiveFailures` value `autoCompactIfNeeded` threads back to the
    /// caller (`autoCompact.ts:328-349`): reset to `0` on a successful
    /// autocompact, incremented on a failed one, and carried through unchanged
    /// when autocompact did not run. The caller persists this into its
    /// `AutoCompactTrackingState` so the next iteration's circuit breaker sees
    /// it.
    pub consecutive_failures: u32,
    /// Whether the autocompact layer actually ran and succeeded this pass.
    /// Mirrors `wasCompacted` from `autoCompactIfNeeded`. `false` when
    /// autocompact was skipped (under threshold or circuit-breaker tripped) or
    /// failed; snip/micro firing alone does **not** set this.
    pub was_compacted: bool,
    /// `true` when the rapid-refill (thrashing) breaker tripped this pass:
    /// the context refilled to the limit within
    /// [`RAPID_REFILL_TURN_WINDOW`](crate::thresholds::RAPID_REFILL_TURN_WINDOW)
    /// turns of the previous compact,
    /// [`MAX_CONSECUTIVE_RAPID_REFILLS`] times in a row. When set, the
    /// summarizer was SKIPPED (history untouched, `was_compacted == false`) and
    /// the caller should emit `tengu_auto_compact_rapid_refill_breaker` /
    /// (reactive PTL path) surface the thrashing message. Mirrors
    /// `rapidRefillBreakerTripped` from `autoCompactIfNeeded`
    /// (`bin/claude.exe` offset 203006250).
    pub rapid_refill_breaker_tripped: bool,
    /// The rapid-refill count carried forward into the next turn's tracking
    /// state (`consecutiveRapidRefills`): the `kho` result this pass. On a
    /// successful compact this is written into
    /// [`AutoCompactTrackingState::consecutive_rapid_refills`] (alongside
    /// `compacted=true`, `turn_counter=0`) so the next refill within the window
    /// increments it.
    pub consecutive_rapid_refills: u32,
    /// #58: the usage-zeroed verbatim tail the autocompact layer preserved
    /// (`messagesToPreserve` → `messagesToKeep`). Carried separately from
    /// [`Self::messages`] (which holds the leading `summaryMessages`) so the
    /// orchestrator's `apply_post_compact` can splice it AFTER the summary in
    /// the `[boundary, ...summary, ...messagesToKeep, ...sidecars, ...attachments]`
    /// order and populate the boundary's `preserved_segment`. Empty unless the
    /// autocompact layer fired AND a tail was preserved — so the snip/micro-only
    /// and under-threshold paths leave it empty (history shape unchanged).
    pub messages_to_preserve: Vec<ConversationMessage>,
    /// Internal vision sidecars from the compacted prefix. These messages are
    /// carried outside the summary so compaction cannot silently discard the
    /// structured fingerprints that make historical media reusable.
    pub media_analysis_to_preserve: Vec<ConversationMessage>,
    /// Usage incurred by the summary side-query. `None` when no LLM
    /// compaction ran (snip/micro/under-threshold paths).
    pub compaction_usage: Option<Usage>,
    /// Model configured for the summary side-query, paired with
    /// [`Self::compaction_usage`] for cost attribution.
    pub compaction_model: Option<String>,
    /// Profile the summary side-query was routed through, completing
    /// [`Self::compaction_model`]'s cost attribution.
    pub compaction_profile: Option<String>,
}

/// Owns one instance of each layer + the autocompact threshold.
pub struct CompactionOrchestrator {
    /// Snip layer.
    pub snip: SnipCompactor,
    /// Microcompact layer.
    pub micro: Microcompactor,
    /// Same-input cache for microcompact results.
    pub cached_micro: CachedMicrocompact,
    /// Read-time context-collapse commit/staging store.
    pub context_collapse: ContextCollapse,
    /// Autocompact layer.
    pub auto: Autocompactor,
    /// Token threshold above which autocompact fires. Interior-mutable because
    /// [`Self::refresh_autocompact_threshold`] can move it between calls when
    /// the orchestrator was built with [`Self::with_model_derived_threshold`];
    /// read it through [`Self::autocompact_threshold`].
    autocompact_threshold: AtomicU64,
    /// `true` when the threshold follows the session's current model instead of
    /// the value passed to the constructor. See
    /// [`Self::with_model_derived_threshold`].
    model_derived_threshold: bool,
}

impl CompactionOrchestrator {
    /// Build a fresh orchestrator with default per-layer config.
    #[must_use]
    pub fn new(autocompact_threshold: u64) -> Self {
        Self {
            snip: SnipCompactor,
            micro: Microcompactor {
                config: TimeBasedMCConfig::default(),
            },
            cached_micro: CachedMicrocompact::default(),
            context_collapse: ContextCollapse::default(),
            auto: Autocompactor::new(),
            autocompact_threshold: AtomicU64::new(autocompact_threshold),
            model_derived_threshold: false,
        }
    }

    /// Build an orchestrator with a caller-provided [`Autocompactor`], keeping
    /// the default snip + microcompact layers.
    ///
    /// The composition root (In-Loop Compaction Batch 6) passes an autocompactor
    /// wired via [`Autocompactor::with_forked_runner`] so the autocompact layer
    /// issues a real forked summary call (sharing the parent's prompt cache)
    /// instead of the deterministic fallback that [`Self::new`]'s
    /// [`Autocompactor::new`] produces.
    #[must_use]
    pub fn with_autocompactor(auto: Autocompactor, autocompact_threshold: u64) -> Self {
        Self {
            snip: SnipCompactor,
            micro: Microcompactor {
                config: TimeBasedMCConfig::default(),
            },
            cached_micro: CachedMicrocompact::default(),
            context_collapse: ContextCollapse::default(),
            auto,
            autocompact_threshold: AtomicU64::new(autocompact_threshold),
            model_derived_threshold: false,
        }
    }

    /// Follow the session's current model: before every proactive pass,
    /// [`Self::refresh_autocompact_threshold`] re-resolves the autocompact
    /// threshold from the model the session is on NOW.
    ///
    /// claude-code resolves `getAutoCompactThreshold(model)`
    /// (`autoCompact.ts:72-91`) at its `autoCompactIfNeeded` call site, not once
    /// at construction. A fixed ceiling baked in at boot is wrong for any
    /// session that switches models, and for every non-Claude catalog model —
    /// their real window comes from the model registry, not the 200k Claude
    /// default the fixed constants were sized against. Callers that pin an
    /// explicit threshold (tests, embedded hosts) keep the constructor value
    /// verbatim.
    #[must_use]
    pub fn with_model_derived_threshold(mut self) -> Self {
        self.model_derived_threshold = true;
        self
    }

    /// The autocompact threshold in force for the next pass.
    #[must_use]
    pub fn autocompact_threshold(&self) -> u64 {
        self.autocompact_threshold.load(Ordering::Relaxed)
    }

    /// Re-resolve the autocompact threshold for `model` and return it.
    ///
    /// A no-op unless the orchestrator was built with
    /// [`Self::with_model_derived_threshold`]: pinned-threshold callers get
    /// their configured value back untouched. Cheap enough to run before every
    /// proactive pass.
    pub fn refresh_autocompact_threshold(&self, model: &str, betas: &[String]) -> u64 {
        if self.model_derived_threshold {
            self.autocompact_threshold.store(
                crate::thresholds::auto_compact_threshold(model, betas),
                Ordering::Relaxed,
            );
        }
        self.autocompact_threshold()
    }

    /// Run one full orchestrator pass with a **fresh** tracking state.
    ///
    /// Thin wrapper over [`Self::process_iteration_tracked`] that starts from a
    /// default [`AutoCompactTrackingState`] (zero consecutive failures), so the
    /// circuit breaker never short-circuits on this path. This is the automatic
    /// compatibility wrapper; explicit `/compact` uses [`Self::process_forced`]
    /// so it bypasses the automatic token threshold and summarizes full history.
    ///
    /// `snip_tokens_freed_already` lets the caller report snip work done outside
    /// this entry point; it is folded into the freed total and the
    /// should-auto-compact subtraction.
    ///
    /// # Errors
    ///
    /// Propagates [`CompactionError`] from the autocompact layer when it fires
    /// and fails.
    pub async fn process_iteration(
        &self,
        messages: Vec<ConversationMessage>,
        snip_tokens_freed_already: u64,
    ) -> Result<IterationCompactionResult, CompactionError> {
        let mut tracking = AutoCompactTrackingState::default();
        self.process_iteration_tracked(messages, snip_tokens_freed_already, &mut tracking)
            .await
    }

    /// Run a user-requested `/compact` pass unconditionally.
    ///
    /// Manual compaction is deliberately separate from the automatic
    /// threshold/circuit-breaker pipeline: Claude Code invokes the summarizer
    /// whenever the history has a valid prefix/tail split, even when the
    /// context is far below the automatic threshold. It preserves recent API
    /// rounds when possible; 2.1.286 also permits a short-history summarize-all
    /// fallback. Fewer than two groups returns the byte-exact user-facing error.
    pub async fn process_forced(
        &self,
        messages: Vec<ConversationMessage>,
        custom_instructions: Option<&str>,
    ) -> Result<IterationCompactionResult, CompactionError> {
        if crate::grouping::group_messages_by_api_round(&messages).len() < 2 {
            return Err(CompactionError::NotEnoughMessages);
        }

        let media_analysis_to_preserve = media_analysis_messages(&messages);
        let result = self
            .auto
            .compact_manual_with_instructions(messages, custom_instructions)
            .await?;
        let total_tokens_freed = result
            .pre_compact_token_count
            .saturating_sub(result.post_compact_token_count);

        Ok(IterationCompactionResult {
            messages: result.summary_messages,
            raw_summary_text: result.raw_summary_text,
            layers_applied: vec![CompactionLayer::Autocompact],
            total_tokens_freed,
            cache_hit: false,
            consecutive_failures: 0,
            was_compacted: true,
            rapid_refill_breaker_tripped: false,
            consecutive_rapid_refills: 0,
            messages_to_preserve: result.messages_to_preserve,
            media_analysis_to_preserve,
            compaction_usage: result.compaction_usage,
            compaction_model: Some(result.summary_model),
            compaction_profile: result.summary_profile,
        })
    }

    /// Summarize one side of a `/rewind` message-selector split — oracle `zir`.
    ///
    /// `context` is what the summarizer sees; `summarized` is the half the
    /// summary actually covers. They differ for
    /// [`crate::prompt::SummarizeDirection::From`], which shows the model the
    /// whole conversation so it can tell the recent portion from the earlier
    /// one.
    ///
    /// ⚠️ No `group_messages_by_api_round(..).len() < 2` guard, unlike
    /// [`Self::process_forced`]: the user picked this split point, and
    /// upstream's only precondition is that the summarize side is non-empty
    /// (checked in [`crate::selector::split_at`]). Refusing a two-message
    /// selection here would reject a choice the picker offered.
    ///
    /// # Errors
    /// Propagates [`CompactionError`] from the autocompact layer.
    pub async fn summarize_selection(
        &self,
        context: Vec<ConversationMessage>,
        summarized: &[ConversationMessage],
        custom_instructions: Option<&str>,
        direction: crate::prompt::SummarizeDirection,
    ) -> Result<IterationCompactionResult, CompactionError> {
        // Vision sidecars from the SUMMARIZED half only: the kept half still
        // carries its own, and `apply_post_compact` de-duplicates against it.
        let media_analysis_to_preserve = media_analysis_messages(summarized);
        let result = self
            .auto
            .summarize_selection(context, custom_instructions, direction)
            .await?;
        let total_tokens_freed = result
            .pre_compact_token_count
            .saturating_sub(result.post_compact_token_count);

        Ok(IterationCompactionResult {
            messages: result.summary_messages,
            raw_summary_text: result.raw_summary_text,
            layers_applied: vec![CompactionLayer::Autocompact],
            total_tokens_freed,
            cache_hit: false,
            consecutive_failures: 0,
            was_compacted: true,
            rapid_refill_breaker_tripped: false,
            consecutive_rapid_refills: 0,
            messages_to_preserve: result.messages_to_preserve,
            media_analysis_to_preserve,
            compaction_usage: result.compaction_usage,
            compaction_model: Some(result.summary_model),
            compaction_profile: result.summary_profile,
        })
    }

    /// Rescue an actual provider overflow even when the local estimate is
    /// below the automatic threshold. Failed attempts leave input history intact.
    pub async fn process_reactive_tracked(
        &self,
        messages: Vec<ConversationMessage>,
        tracking: &mut AutoCompactTrackingState,
        custom_instructions: Option<&str>,
        initial_token_gap: Option<u64>,
    ) -> Result<IterationCompactionResult, CompactionError> {
        let rapid_refill = rapid_refill_count(tracking);
        if rapid_refill >= MAX_CONSECUTIVE_RAPID_REFILLS {
            return Ok(IterationCompactionResult {
                messages,
                raw_summary_text: String::new(),
                layers_applied: Vec::new(),
                total_tokens_freed: 0,
                cache_hit: false,
                consecutive_failures: tracking.consecutive_failures,
                was_compacted: false,
                rapid_refill_breaker_tripped: true,
                consecutive_rapid_refills: rapid_refill,
                messages_to_preserve: Vec::new(),
                media_analysis_to_preserve: Vec::new(),
                compaction_usage: None,
                compaction_model: None,
                compaction_profile: None,
            });
        }
        let media_analysis_to_preserve = media_analysis_messages(&messages);
        let result = self
            .auto
            .compact_reactive_with_instructions(messages, custom_instructions, initial_token_gap)
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                tracking.consecutive_failures = tracking.consecutive_failures.saturating_add(1);
                return Err(error);
            }
        };
        tracking.consecutive_failures = 0;
        tracking.compacted = true;
        tracking.turn_counter = 0;
        tracking.consecutive_rapid_refills = rapid_refill;
        Ok(IterationCompactionResult {
            total_tokens_freed: result
                .pre_compact_token_count
                .saturating_sub(result.post_compact_token_count),
            messages: result.summary_messages,
            raw_summary_text: result.raw_summary_text,
            layers_applied: vec![CompactionLayer::Autocompact],
            cache_hit: false,
            consecutive_failures: 0,
            was_compacted: true,
            rapid_refill_breaker_tripped: false,
            consecutive_rapid_refills: rapid_refill,
            messages_to_preserve: result.messages_to_preserve,
            media_analysis_to_preserve,
            compaction_usage: result.compaction_usage,
            compaction_model: Some(result.summary_model),
            compaction_profile: result.summary_profile,
        })
    }

    /// Run one full orchestrator pass, threading `tracking` for the autocompact
    /// circuit breaker.
    ///
    /// Order (TS `query.ts:400-467`):
    /// 1. **Snip** — drop oldest messages targeting the autocompact threshold
    ///    (cheapest, no LLM). Records [`CompactionLayer::Snip`] when it removed
    ///    at least one message.
    /// 2. **Microcompact** — clear large tool results. Records
    ///    [`CompactionLayer::Microcompact`] when it cleared anything.
    /// 3. **Autocompact** — only when still over threshold per
    ///    [`should_auto_compact`](crate::threshold_calc::should_auto_compact)
    ///    **and** the circuit breaker has not tripped
    ///    (`tracking.consecutive_failures < MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES`).
    ///    On success resets `tracking.consecutive_failures` to `0` and sets
    ///    `was_compacted = true`; on error increments it and propagates the
    ///    error.
    ///
    /// `snip_tokens_freed_already` accounts for snip work the caller already did
    /// before this entry point (TS `snipTokensFreed`); it is added to this
    /// pass's snip savings for the threshold subtraction.
    ///
    /// # Errors
    ///
    /// Propagates [`CompactionError`] from the autocompact layer when it fires
    /// and fails. On error `tracking.consecutive_failures` has already been
    /// incremented.
    pub async fn process_iteration_tracked(
        &self,
        messages: Vec<ConversationMessage>,
        snip_tokens_freed_already: u64,
        tracking: &mut AutoCompactTrackingState,
    ) -> Result<IterationCompactionResult, CompactionError> {
        self.process_iteration_tracked_with_instructions(
            messages,
            snip_tokens_freed_already,
            tracking,
            None,
        )
        .await
    }

    /// Automatic compaction pass with optional instructions contributed by a
    /// successful `PreCompact` hook. Existing automatic callers use
    /// [`Self::process_iteration_tracked`]; lifecycle-aware callers use this
    /// seam after executing hooks.
    pub async fn process_iteration_tracked_with_instructions(
        &self,
        messages: Vec<ConversationMessage>,
        snip_tokens_freed_already: u64,
        tracking: &mut AutoCompactTrackingState,
        custom_instructions: Option<&str>,
    ) -> Result<IterationCompactionResult, CompactionError> {
        self.process_iteration_tracked_with_instructions_and_timing(
            messages,
            snip_tokens_freed_already,
            tracking,
            custom_instructions,
            None,
            SystemTime::now(),
        )
        .await
    }

    /// Automatic compaction with the session's out-of-band assistant timing.
    ///
    /// A missing timestamp is deliberately fail-safe: legacy transcripts do
    /// not run time-based microcompact merely because enough tool results exist.
    pub async fn process_iteration_tracked_with_instructions_and_timing(
        &self,
        mut messages: Vec<ConversationMessage>,
        snip_tokens_freed_already: u64,
        tracking: &mut AutoCompactTrackingState,
        custom_instructions: Option<&str>,
        last_assistant_at: Option<SystemTime>,
        now: SystemTime,
    ) -> Result<IterationCompactionResult, CompactionError> {
        let media_analysis_to_preserve = media_analysis_messages(&messages);
        let mut layers = Vec::new();
        let mut freed = snip_tokens_freed_already;
        let mut cache_hit = false;
        if snip_tokens_freed_already > 0 {
            layers.push(CompactionLayer::Snip);
        }

        // --- Layer 1: snip (cheapest, no LLM) ----------------------------- //
        // COMPACT.4: TS gates the snip pass behind `feature('HISTORY_SNIP')`
        // (`query.ts:401`), which resolves to `envBool('LINGXI_HISTORY_SNIP',
        // false)` (`shims/bun-bundle.ts:20`) — OFF by default in the reference
        // checkout. The prior code ran the snip pass UNCONDITIONALLY, shedding
        // the oldest messages on every iteration even with the feature off; gate
        // it so the default matches TS.
        //
        // Target the autocompact threshold as the snip budget: snip is the
        // cheap first escalation, so we let it shed the oldest messages toward
        // the same ceiling autocompact defends. The protected-tail floor in
        // `SnipCompactor::snip` means snip cannot drive a genuinely-oversized
        // history below the threshold on its own, so autocompact still
        // escalates when warranted (this preserves the manual `/compact`
        // outcome — see `process_iteration`).
        if history_snip_enabled() {
            let current_tokens = crate::grouping::estimate_tokens_for_range(&messages);
            let snip = SnipCompactor::snip(messages, current_tokens, self.autocompact_threshold());
            if snip.removed_count > 0 {
                layers.push(CompactionLayer::Snip);
                freed = freed.saturating_add(snip.tokens_freed);
            }
            messages = snip.messages;
        }

        // --- Layer 2: microcompact (time-gap gated) ----------------------- //
        // CSM.3: TS `maybeTimeBasedMicrocompact` only clears old tool results
        // when the idle-gap trigger fires; with `config.enabled == false` (the
        // default) it is a no-op. The prior code ran `micro.compact`
        // UNCONDITIONALLY, over-clearing tool results on every iteration even
        // when time-based micro is disabled. Per-message wire timestamps remain
        // unchanged; callers provide the persisted session timing sidecar.
        // Missing timing (legacy transcript) is a strict no-op.
        if crate::microcompact::evaluate_time_based_trigger(
            &self.micro.config,
            last_assistant_at,
            now,
        )
        .is_some()
        {
            let micro_key = (
                self.micro.config.enabled,
                self.micro.config.gap_threshold_minutes,
                self.micro.config.keep_recent,
            );
            let cached =
                self.cached_micro
                    .compact_with_key(messages, micro_key, now, |input, now| {
                        self.micro.compact(input, now)
                    });
            cache_hit = cached.cache_hit;
            let micro = cached.result;
            if micro.cleared_count > 0 {
                layers.push(CompactionLayer::Microcompact);
                freed = freed.saturating_add(micro.tokens_saved);
                // COMPACT.3: TS `maybeTimeBasedMicrocompact` suppresses the
                // "context left until autocompact" warning once it has actually
                // cleared tool results (`microCompact.ts:511`, reached only when
                // `tokensSaved > 0` — it returns null at :494-496 otherwise).
                // The token counts are stale until the next API response, so the
                // warning would be misleading. The prior code never suppressed.
                crate::warning_state::suppress_compact_warning();
            }
            messages = micro.messages;
        }

        // Context collapse is a read-time projection over the untouched REPL
        // history. It is applied by the conversation layer immediately before
        // model calls (and by its retry-safe history rewriter), not folded into
        // this mutation-oriented result. Keeping it outside this return value is
        // what prevents committed summaries from leaking into session history.

        // --- Layer 3: autocompact (threshold + circuit-breaker gated) ----- //
        let mut was_compacted = false;
        let mut compaction_usage = None;
        let mut compaction_model = None;
        let mut compaction_profile = None;
        let mut raw_summary_text = String::new();
        // #58: the preserved tail the autocompact layer carries out, if any.
        // Empty unless autocompact fires AND `DRn` selected a preservable tail.
        let mut messages_to_preserve: Vec<ConversationMessage> = Vec::new();
        let estimate_after_micro = crate::grouping::estimate_tokens_for_range(&messages);
        let over_threshold = crate::threshold_calc::should_auto_compact(
            estimate_after_micro,
            // Snip removed messages but the surviving usage estimate above
            // already reflects the post-snip set, so no extra subtraction is
            // applied here (snip_freed = 0). The freed total still carries the
            // savings for the caller's accounting.
            0,
            self.autocompact_threshold(),
        );
        // Circuit breaker: after N consecutive failures, stop trying so a
        // hopelessly-over-limit session does not hammer the summarizer every
        // turn (`autoCompact.ts:260-265`).
        let breaker_tripped = tracking.consecutive_failures >= MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES;

        // #54 rapid-refill (thrashing) breaker: compute `kho(tracking)` BEFORE
        // the summarizer. If the context has refilled to the limit within
        // `RAPID_REFILL_TURN_WINDOW` turns of the previous compact,
        // `MAX_CONSECUTIVE_RAPID_REFILLS` times in a row, SKIP the summarizer
        // entirely — re-summarizing cannot help (a single file/tool output is
        // too large), and re-running it every turn would thrash. Mirrors the
        // proactive trip in `Eho`/`autoCompactIfNeeded` (`bin/claude.exe` offset
        // 203006250): `let d=kho(o); if(d>=f6n) return {wasCompacted:!1,
        // rapidRefillBreakerTripped:!0}`.
        let rapid_refill = rapid_refill_count(tracking);
        let mut rapid_refill_breaker_tripped = false;

        if over_threshold && rapid_refill >= MAX_CONSECUTIVE_RAPID_REFILLS {
            // Breaker trips: skip the summarizer, leave history untouched. The
            // caller emits telemetry / (reactive) surfaces the thrashing
            // message. The rapid-refill count is carried back so the caller can
            // populate `tengu_auto_compact_rapid_refill_breaker`.
            rapid_refill_breaker_tripped = true;
        } else if over_threshold && !breaker_tripped {
            match self
                .auto
                .compact_with_instructions(messages.clone(), custom_instructions)
                .await
            {
                Ok(result) => {
                    raw_summary_text = result.raw_summary_text;
                    compaction_usage = result.compaction_usage;
                    compaction_model = Some(result.summary_model.clone());
                    compaction_profile = result.summary_profile;
                    messages.clone_from(&result.summary_messages);
                    // #58: carry the preserved tail out separately (NOT folded
                    // into `messages`, which is the leading summary set).
                    messages_to_preserve = result.messages_to_preserve;
                    freed = freed.saturating_add(
                        result
                            .pre_compact_token_count
                            .saturating_sub(result.post_compact_token_count),
                    );
                    layers.push(CompactionLayer::Autocompact);
                    was_compacted = true;
                    // Reset the failure count on success.
                    tracking.consecutive_failures = 0;
                    // #54 post-compact bookkeeping (`oe={compacted:!0,turnId:…,
                    // turnCounter:0,consecutiveFailures:0,consecutiveRapidRefills:pe}`,
                    // offset 202919683): mark compacted, reset the turn counter,
                    // and carry the rapid-refill count forward so a refill within
                    // the window on the NEXT compact increments it.
                    tracking.compacted = true;
                    tracking.turn_counter = 0;
                    tracking.consecutive_rapid_refills = rapid_refill;
                }
                Err(e) => {
                    // Increment for the circuit breaker, then propagate.
                    tracking.consecutive_failures = tracking.consecutive_failures.saturating_add(1);
                    return Err(e);
                }
            }
        }

        Ok(IterationCompactionResult {
            messages,
            raw_summary_text,
            layers_applied: layers,
            total_tokens_freed: freed,
            cache_hit,
            consecutive_failures: tracking.consecutive_failures,
            was_compacted,
            rapid_refill_breaker_tripped,
            consecutive_rapid_refills: tracking.consecutive_rapid_refills,
            messages_to_preserve,
            media_analysis_to_preserve,
            compaction_usage,
            compaction_model,
            compaction_profile,
        })
    }
}

/// Whether the `HISTORY_SNIP` snip pass runs (COMPACT.4).
///
/// Mirrors TS `feature('HISTORY_SNIP')`, which resolves to
/// `envBool('LINGXI_HISTORY_SNIP', false)` (`shims/bun-bundle.ts:20,33-37`):
/// the env var must be exactly `"1"` or `"true"` (byte-exact — `envBool` does NOT
/// trim or case-fold, unlike `isEnvTruthy`). Absent or any other value → `false`,
/// matching the reference checkout's default-off so the snip layer is a no-op
/// unless explicitly enabled.
fn history_snip_enabled() -> bool {
    match std::env::var("LINGXI_HISTORY_SNIP") {
        Ok(v) => v == "1" || v == "true",
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autocompact::CompactionResult;
    use async_trait::async_trait;
    use lingxi_core::types::{ContentBlock, MediaAnalysis, MessageId, ToolUseId};
    use serde_json::json;
    use sidequery::{
        CacheSafeParams, CacheSafeParamsSlot, ForkedAgentRunner, SideQueryClient, SideQueryError,
        SideQueryRequest, SideQueryResponse,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use tool_api::context::ToolUseOptions;

    struct SummaryClient;

    #[test]
    fn media_analysis_messages_preserves_only_internal_sidecars() {
        let analysis = MediaAnalysis {
            question_key: "question".into(),
            media_fingerprints: vec!["sha256:abc".into()],
            model: "vision-delegate".into(),
            prompt_version: 1,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            task_findings: vec!["a finding".into()],
            media: Vec::new(),
            cross_media_findings: Vec::new(),
            truncated: false,
        };
        let message = ConversationMessage::User { api_message_override: None,
            id: MessageId::new(),
            content: vec![
                ContentBlock::Text {
                    text: "user text".into(), citations: None,
                },
                ContentBlock::MediaAnalysis {
                    analysis: analysis.clone(),
                },
            ],
            is_meta: true,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };

        let preserved = media_analysis_messages(&[
            message,
            ConversationMessage::user(MessageId::new(), "ordinary user message".into()),
        ]);
        assert_eq!(preserved.len(), 1);
        let ConversationMessage::User {
            content, is_meta, ..
        } = &preserved[0]
        else {
            panic!("expected preserved user sidecar");
        };
        assert!(*is_meta);
        assert_eq!(content, &vec![ContentBlock::MediaAnalysis { analysis }]);
    }

    #[async_trait]
    impl SideQueryClient for SummaryClient {
        async fn query(
            &self,
            _request: SideQueryRequest,
        ) -> Result<SideQueryResponse, SideQueryError> {
            Ok(SideQueryResponse {
                text: Some("<summary>test summary</summary>".into()),
                structured: None,
                tool_calls: Vec::new(),
                usage: Usage::default(),
                stop_reason: Some("end_turn".into()),
                retry_count: 0,
            })
        }
    }

    async fn forced_orchestrator(threshold: u64) -> CompactionOrchestrator {
        let slot = Arc::new(CacheSafeParamsSlot::new());
        slot.save(CacheSafeParams {
            system_prompt: Arc::from("test system"),
            user_context: HashMap::new(),
            system_context: HashMap::new(),
            user_context_message: None,
            tool_use_options: ToolUseOptions {
                debug: false,
                verbose: false,
                main_loop_model: "test-model".into(),
                model_profile: None,
                max_budget_nano_usd: None,
                mcp_clients: Vec::new(),
                is_non_interactive_session: false,
                custom_system_prompt: None,
                append_system_prompt: None,
            },
            fork_context_messages: Vec::new(),
            transcript_path: None,
            generation: 0,
            tools: Vec::new(),
            effort: None,
        })
        .await;
        let runner = Arc::new(
            ForkedAgentRunner::new()
                .with_side_query_client(Arc::new(SummaryClient), "test-model".into()),
        );
        CompactionOrchestrator::with_autocompactor(
            Autocompactor::with_forked_runner(runner, slot),
            threshold,
        )
    }

    fn long_user(i: usize) -> ConversationMessage {
        // ~80 chars → ~20 tokens each, so a handful clears any small threshold.
        ConversationMessage::user(
            MessageId::new(),
            format!(
                "turn-{i} padding text to push the token estimate over a small threshold value"
            ),
        )
    }

    fn assistant(text: &str) -> ConversationMessage {
        ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Text { text: text.into(), citations: None }],
            stop_reason: Some("end_turn".into()),
        }
    }

    /// Snapshot + clear the window-related env vars under [`ENV_LOCK`], run
    /// `body`, then restore. Mirrors `thresholds::tests::with_clean_env`, which
    /// is private to that module.
    fn with_clean_window_env(body: impl FnOnce()) {
        const KEYS: &[&str] = &[
            "LINGXI_AUTO_COMPACT_WINDOW",
            "LINGXI_AUTOCOMPACT_PCT_OVERRIDE",
            "LINGXI_MAX_CONTEXT_TOKENS",
            "LINGXI_MAX_OUTPUT_TOKENS",
            "USER_TYPE",
        ];
        let _guard = crate::thresholds::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved: Vec<(&str, Option<String>)> =
            KEYS.iter().map(|&k| (k, std::env::var(k).ok())).collect();
        for &k in KEYS {
            std::env::remove_var(k);
        }
        body();
        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn model_derived_threshold_follows_the_model_only_when_opted_in() {
        with_clean_window_env(|| {
            // A 1M-token catalog window with a 384k output cap: the threshold is
            // 1_000_000 − min(384_000, MAX_OUTPUT_TOKENS_FOR_SUMMARY) − buffer
            // = 1_000_000 − 32_000 − 13_000.
            const MODEL: &str = "test-model-derived-threshold";
            llm_runtime::model::model_limits::register(
                MODEL,
                llm_runtime::model::model_limits::ModelLimits {
                    context_window: 1_000_000,
                    max_output_tokens: 384_000,
                },
            );

            // A pinned threshold survives the refresh untouched.
            let pinned = CompactionOrchestrator::new(150_000);
            assert_eq!(pinned.refresh_autocompact_threshold(MODEL, &[]), 150_000);
            assert_eq!(pinned.autocompact_threshold(), 150_000);

            // Opted in, the threshold follows the model the session is on.
            let derived = CompactionOrchestrator::new(150_000).with_model_derived_threshold();
            assert_eq!(derived.refresh_autocompact_threshold(MODEL, &[]), 955_000);
            assert_eq!(derived.autocompact_threshold(), 955_000);
        });
    }

    #[tokio::test]
    async fn forced_compact_ignores_automatic_threshold() {
        let orch = forced_orchestrator(u64::MAX).await;
        let messages = vec![
            long_user(0),
            assistant("first reply"),
            long_user(1),
            assistant("second reply"),
        ];

        let result = orch
            .process_forced(messages, None)
            .await
            .expect("manual compact must run below the auto threshold");

        assert!(result.was_compacted);
        assert_eq!(result.layers_applied, vec![CompactionLayer::Autocompact]);
    }

    #[tokio::test]
    async fn forced_compact_rejects_history_without_a_completed_reply() {
        let orch = forced_orchestrator(0).await;
        let err = orch
            .process_forced(vec![long_user(0)], None)
            .await
            .expect_err("a lone user message is not enough");
        assert!(matches!(err, CompactionError::NotEnoughMessages));
        assert_eq!(err.to_string(), "Not enough messages to compact.");
    }

    /// The 2.1.286 Hre last resort summarizes a complete single exchange.
    #[tokio::test]
    async fn forced_compact_summarizes_one_complete_exchange() {
        let orch = forced_orchestrator(u64::MAX).await;
        let result = orch
            .process_forced(vec![long_user(0), assistant("only reply")], None)
            .await
            .expect("286 summarize_all covers one complete exchange");
        assert!(result.was_compacted);
        assert!(result.messages_to_preserve.is_empty());
    }

    /// Binary-verified: the manual path preserves the LAST API-round group
    /// verbatim (`Nto`: `messagesToPreserve: m.flat()` with `s = 1`), exactly
    /// like the reactive path — it is NOT the full-history `messagesToKeep: []`
    /// shape. `[u0,a0,u1,a1]` groups as `[u0],[a0,u1],[a1]`; the preserved tail
    /// is the final `[a1]` group.
    #[tokio::test]
    async fn forced_compact_preserves_the_last_api_round_group() {
        let orch = forced_orchestrator(u64::MAX).await;
        let result = orch
            .process_forced(
                vec![
                    long_user(0),
                    assistant("first reply"),
                    long_user(1),
                    assistant("second reply"),
                ],
                None,
            )
            .await
            .expect("two rounds compact");

        assert!(result.was_compacted);
        assert_eq!(
            result.messages_to_preserve.len(),
            1,
            "the last assistant-led group rides verbatim after the summary"
        );
        match &result.messages_to_preserve[0] {
            ConversationMessage::Assistant { content, .. } => {
                assert!(matches!(
                    &content[0],
                    ContentBlock::Text { text, .. } if text == "second reply"
                ));
            }
            other => panic!("preserved tail must be the final assistant reply, got {other:?}"),
        }
    }

    /// An `Autocompactor` whose `compact` always returns `Err`, to exercise the
    /// circuit breaker without a network call. Built by swapping the orchestrator
    /// field after construction.
    fn failing_autocompactor() -> Autocompactor {
        // The default (unwired) autocompactor SUCCEEDS via the deterministic
        // fallback, so to force failures we wire a runner with an empty slot:
        // `compact` then returns `CompactionError::Internal("no cache-safe
        // params")` on every call.
        use sidequery::{CacheSafeParamsSlot, ForkedAgentRunner};
        use std::sync::Arc;
        // No `with_side_query_client` → run() would fail too, but the empty slot
        // is consulted first and short-circuits with an error before any call.
        let runner = Arc::new(ForkedAgentRunner::new());
        let slot = Arc::new(CacheSafeParamsSlot::new()); // never saved → empty
        Autocompactor::with_forked_runner(runner, slot)
    }

    /// A panicking autocompactor: any call to `compact` would panic. Used to
    /// prove the snip-only-under-threshold path never reaches autocompact.
    ///
    /// We cannot make `Autocompactor::compact` itself panic (it is a concrete
    /// type), so instead we assert via the threshold gate: with a threshold
    /// above the post-snip estimate, `should_auto_compact` is false and the
    /// `auto.compact` arm is never entered. The default autocompactor here would
    /// otherwise succeed; the test asserts it was NOT invoked by checking that
    /// `was_compacted` is false and `Autocompact` is absent from the layers.
    fn order_orchestrator(threshold: u64) -> CompactionOrchestrator {
        CompactionOrchestrator::new(threshold)
    }

    fn assistant_tool_use(name: &str, id: ToolUseId) -> ConversationMessage {
        ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::ToolUse { input_projection: None,
                id,
                name: name.into(),
                input: json!({}),
                provider_id: None,
            }],
            stop_reason: None,
        }
    }

    fn user_tool_result(tool_use_id: ToolUseId, content: &str) -> ConversationMessage {
        ConversationMessage::User { api_message_override: None,
            id: MessageId::new(),
            content: vec![ContentBlock::ToolResult { content_projection: None,
                tool_use_id,
                content: content.into(),
                is_error: Some(false),
                provider_tool_use_id: None,
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        }
    }

    fn microcompactable_messages() -> Vec<ConversationMessage> {
        let mut msgs = Vec::new();
        for i in 0..8 {
            let id = ToolUseId::new();
            msgs.push(assistant_tool_use("Read", id.clone()));
            msgs.push(user_tool_result(
                id,
                &format!("body-{i} {}", "x".repeat(40_000)),
            ));
        }
        msgs
    }

    #[tokio::test]
    async fn circuit_breaker_short_circuits_after_three_failures() {
        // Threshold 0 so `should_auto_compact` is always true: every pass tries
        // autocompact. The failing autocompactor errors each time, so the first
        // three passes return Err and bump consecutive_failures to 3; the fourth
        // short-circuits (was_compacted=false) WITHOUT calling the summarizer.
        let mut orch = order_orchestrator(0);
        orch.auto = failing_autocompactor();

        let mut tracking = AutoCompactTrackingState::default();
        let msgs = vec![long_user(0), long_user(1), long_user(2)];

        // 3 consecutive Err from the failing mock summarizer.
        for expected in 1..=3u32 {
            let r = orch
                .process_iteration_tracked(msgs.clone(), 0, &mut tracking)
                .await;
            assert!(r.is_err(), "attempt {expected} should fail");
            assert_eq!(
                tracking.consecutive_failures, expected,
                "failure count should increment to {expected}"
            );
        }

        // 4th call: breaker tripped → short-circuit, no summarizer call, Ok with
        // was_compacted=false.
        let res = orch
            .process_iteration_tracked(msgs.clone(), 0, &mut tracking)
            .await
            .expect("breaker short-circuits to Ok, not Err");
        assert!(
            !res.was_compacted,
            "4th call must NOT compact (circuit breaker)"
        );
        assert!(
            !res.layers_applied.contains(&CompactionLayer::Autocompact),
            "autocompact layer must not fire when breaker is tripped"
        );
        assert_eq!(res.consecutive_failures, 3);
    }

    #[tokio::test]
    async fn snip_alone_under_threshold_never_calls_summarizer() {
        // A huge threshold means even the full (un-snipped) history is under it,
        // so `should_auto_compact` is false and the autocompact arm is never
        // entered. We assert no Autocompact layer and was_compacted=false. If the
        // gate were broken and autocompact ran, the default autocompactor would
        // succeed and the layer would appear — so its ABSENCE proves the
        // summarizer was not invoked.
        let orch = order_orchestrator(1_000_000);
        let mut tracking = AutoCompactTrackingState::default();
        let msgs = vec![long_user(0), long_user(1), long_user(2)];

        let res = orch
            .process_iteration_tracked(msgs, 0, &mut tracking)
            .await
            .expect("under threshold → Ok");

        assert!(!res.was_compacted, "must not compact under threshold");
        assert!(
            !res.layers_applied.contains(&CompactionLayer::Autocompact),
            "autocompact must not fire under threshold"
        );
        // No snip either (under budget) and no micro (no tool results).
        assert!(res.layers_applied.is_empty(), "no layers should fire");
        assert_eq!(tracking.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn success_resets_consecutive_failures_and_sets_was_compacted() {
        // Threshold 0 → always over threshold; default (unwired) autocompactor
        // SUCCEEDS via the deterministic fallback. Seed a non-zero failure count
        // (below the breaker limit) and confirm a successful pass resets it and
        // reports was_compacted=true.
        let orch = order_orchestrator(0);
        let mut tracking = AutoCompactTrackingState {
            consecutive_failures: 2,
            ..Default::default()
        };
        let msgs = vec![long_user(0), long_user(1)];

        let res = orch
            .process_iteration_tracked(msgs, 0, &mut tracking)
            .await
            .expect("default autocompactor succeeds");

        assert!(res.was_compacted, "autocompact should have run");
        assert!(res.layers_applied.contains(&CompactionLayer::Autocompact));
        assert_eq!(
            tracking.consecutive_failures, 0,
            "success resets the failure count"
        );
        assert_eq!(res.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn process_iteration_wrapper_uses_fresh_tracking() {
        // The thin wrapper starts from a fresh tracking state, so even after
        // (hypothetical) prior failures elsewhere it always attempts autocompact
        // when over threshold. Threshold 0 + default autocompactor → success.
        let orch = order_orchestrator(0);
        let msgs = vec![long_user(0), long_user(1)];
        let res = orch
            .process_iteration(msgs, 0)
            .await
            .expect("wrapper succeeds");
        assert!(res.was_compacted);
        assert!(res.layers_applied.contains(&CompactionLayer::Autocompact));
    }

    #[tokio::test]
    async fn microcompact_cache_hit_is_reported_on_repeated_same_input() {
        let mut orch = order_orchestrator(1_000_000);
        orch.micro.config.enabled = true;
        let msgs = microcompactable_messages();
        let now = SystemTime::now();
        let last = now.checked_sub(std::time::Duration::from_secs(60 * 60));

        let first = orch
            .process_iteration_tracked_with_instructions_and_timing(
                msgs.clone(),
                0,
                &mut AutoCompactTrackingState::default(),
                None,
                last,
                now,
            )
            .await
            .expect("first microcompact pass succeeds");
        let second = orch
            .process_iteration_tracked_with_instructions_and_timing(
                msgs,
                0,
                &mut AutoCompactTrackingState::default(),
                None,
                last,
                now,
            )
            .await
            .expect("second microcompact pass succeeds");

        assert!(!first.cache_hit, "empty cache starts with a miss");
        assert!(
            second.cache_hit,
            "same input should hit cached microcompact"
        );
        assert_eq!(
            first.messages, second.messages,
            "hit returns cached summary"
        );
        assert!(second
            .layers_applied
            .contains(&CompactionLayer::Microcompact));
    }

    #[tokio::test]
    async fn enabled_microcompact_without_assistant_timestamp_is_fail_safe_noop() {
        let mut orch = order_orchestrator(1_000_000);
        orch.micro.config.enabled = true;
        let msgs = microcompactable_messages();

        let result = orch
            .process_iteration_tracked(msgs.clone(), 0, &mut AutoCompactTrackingState::default())
            .await
            .expect("legacy timing no-op succeeds");

        assert_eq!(result.messages, msgs);
        assert!(!result
            .layers_applied
            .contains(&CompactionLayer::Microcompact));
    }

    // --- COMPACT.4: HISTORY_SNIP gating ---------------------------------- //

    /// Serializes the env-mutating COMPACT.4 tests so the shared
    /// `LINGXI_HISTORY_SNIP` process var isn't raced. The body runs with the
    /// var set to `value` (or removed when `None`); the prior value is always
    /// restored. The body is sync (drives async work via a local runtime) so the
    /// guard is never held across an `.await`.
    static SNIP_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_snip_env<R>(value: Option<&str>, body: impl FnOnce() -> R) -> R {
        let _guard = SNIP_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved = std::env::var("LINGXI_HISTORY_SNIP").ok();
        match value {
            Some(v) => std::env::set_var("LINGXI_HISTORY_SNIP", v),
            None => std::env::remove_var("LINGXI_HISTORY_SNIP"),
        }
        let out = body();
        match saved {
            Some(v) => std::env::set_var("LINGXI_HISTORY_SNIP", v),
            None => std::env::remove_var("LINGXI_HISTORY_SNIP"),
        }
        out
    }

    /// Run `fut` to completion on a fresh current-thread runtime, so an
    /// env-serialized (sync) test can drive the async orchestrator without
    /// holding `SNIP_ENV_LOCK` across an `.await`.
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(fut)
    }

    #[test]
    fn snip_disabled_by_default_does_not_fire_over_budget() {
        // COMPACT.4: with HISTORY_SNIP OFF (the default), the snip pass is a
        // no-op even with a history far over budget — no Snip layer is recorded.
        // Autocompact still escalates because the (un-snipped) history is over
        // threshold.
        with_snip_env(None, || {
            let orch = order_orchestrator(50);
            let mut tracking = AutoCompactTrackingState::default();
            let msgs: Vec<_> = (0..40).map(long_user).collect();

            let res = block_on(orch.process_iteration_tracked(msgs, 0, &mut tracking))
                .expect("over threshold → autocompact succeeds");

            assert!(
                !res.layers_applied.contains(&CompactionLayer::Snip),
                "snip must NOT fire when HISTORY_SNIP is off (default)"
            );
            assert!(
                res.layers_applied.contains(&CompactionLayer::Autocompact),
                "autocompact still escalates on the un-snipped, over-threshold history"
            );
        });
    }

    #[test]
    fn history_snip_env_flag_enables_snip() {
        // COMPACT.4: with LINGXI_HISTORY_SNIP=1 the snip pass runs again,
        // sheds the oldest messages down to the protected tail, and records the
        // Snip layer (mirroring TS `feature('HISTORY_SNIP')` on).
        with_snip_env(Some("1"), || {
            let orch = order_orchestrator(50);
            let mut tracking = AutoCompactTrackingState::default();
            let msgs: Vec<_> = (0..40).map(long_user).collect();

            let res = block_on(orch.process_iteration_tracked(msgs, 0, &mut tracking))
                .expect("over threshold → autocompact succeeds");

            assert!(
                res.layers_applied.contains(&CompactionLayer::Snip),
                "snip should fire when HISTORY_SNIP is enabled and over budget"
            );
            assert!(res.total_tokens_freed > 0, "snip should free tokens");
        });
    }

    #[test]
    fn history_snip_enabled_parses_envbool() {
        // Byte-faithful `envBool('LINGXI_HISTORY_SNIP', false)`: only the
        // exact strings "1" and "true" enable it (no trim / case-fold), absent or
        // anything else → false.
        with_snip_env(None, || assert!(!history_snip_enabled()));
        with_snip_env(Some("1"), || assert!(history_snip_enabled()));
        with_snip_env(Some("true"), || assert!(history_snip_enabled()));
        with_snip_env(Some("0"), || assert!(!history_snip_enabled()));
        with_snip_env(Some("yes"), || assert!(!history_snip_enabled()));
        with_snip_env(Some("TRUE"), || assert!(!history_snip_enabled()));
    }

    // Keep an explicit reference to CompactionResult's shape so the test module
    // documents the success contract it relies on (pre/post token counts feed
    // total_tokens_freed).
    #[allow(dead_code)]
    fn _result_shape(r: &CompactionResult) -> u64 {
        r.pre_compact_token_count
            .saturating_sub(r.post_compact_token_count)
    }

    // --- #54 rapid-refill (thrashing) breaker ---------------------------- //

    #[tokio::test]
    async fn rapid_refill_breaker_skips_summarizer_after_three_in_window() {
        use crate::thresholds::MAX_CONSECUTIVE_RAPID_REFILLS;
        // Threshold 0 → always over threshold. Default autocompactor SUCCEEDS.
        // Simulate three consecutive rapid refills: each pass leaves
        // `compacted=true, turn_counter=0`, so `rapid_refill_count` climbs
        // 1 → 2 → 3; on the pass where it would reach 3 the breaker trips
        // and the summarizer is SKIPPED.
        let orch = order_orchestrator(0);
        let msgs = vec![long_user(0), long_user(1)];

        // Pre-seed tracking as if two rapid refills already happened: the last
        // compact set compacted=true, turn_counter=0, consecutive_rapid_refills=2.
        let mut tracking = AutoCompactTrackingState {
            compacted: true,
            turn_counter: 0,
            consecutive_rapid_refills: 2,
            ..Default::default()
        };

        // This pass: rapid_refill_count = 2 + 1 = 3 >= MAX → breaker trips.
        let res = orch
            .process_iteration_tracked(msgs, 0, &mut tracking)
            .await
            .expect("breaker trips to Ok, not Err");

        assert!(
            res.rapid_refill_breaker_tripped,
            "the thrashing breaker must trip at {MAX_CONSECUTIVE_RAPID_REFILLS} rapid refills"
        );
        assert!(
            !res.was_compacted,
            "the summarizer must be SKIPPED when the breaker trips"
        );
        assert!(
            !res.layers_applied.contains(&CompactionLayer::Autocompact),
            "autocompact must not fire when the rapid-refill breaker is tripped"
        );
    }

    #[tokio::test]
    async fn rapid_refill_does_not_trip_when_window_exceeded() {
        // compacted=true but turn_counter >= window(3) → NOT a rapid refill →
        // count resets to 0, the summarizer runs normally.
        let orch = order_orchestrator(0);
        let msgs = vec![long_user(0), long_user(1)];
        let mut tracking = AutoCompactTrackingState {
            compacted: true,
            turn_counter: 5, // well past the 3-turn window
            consecutive_rapid_refills: 2,
            ..Default::default()
        };

        let res = orch
            .process_iteration_tracked(msgs, 0, &mut tracking)
            .await
            .expect("normal compact");

        assert!(!res.rapid_refill_breaker_tripped, "breaker must NOT trip");
        assert!(res.was_compacted, "summarizer runs when not thrashing");
        // After a successful compact the count is reset (kho returned 0 here).
        assert_eq!(res.consecutive_rapid_refills, 0);
        assert_eq!(tracking.turn_counter, 0, "turn counter resets on compact");
        assert!(tracking.compacted, "compacted flag set after a compact");
    }

    #[tokio::test]
    async fn rapid_refill_count_climbs_across_consecutive_compacts() {
        // Two consecutive in-window compacts: the carried count climbs 1 → 2
        // (still below the breaker), proving the bookkeeping accumulates.
        let orch = order_orchestrator(0);
        let msgs = vec![long_user(0), long_user(1)];
        // First refill within window: prev compacted=true, turn_counter=0,
        // count=0 → kho=1 → compact runs, carries 1.
        let mut tracking = AutoCompactTrackingState {
            compacted: true,
            turn_counter: 0,
            consecutive_rapid_refills: 0,
            ..Default::default()
        };
        let r1 = orch
            .process_iteration_tracked(msgs.clone(), 0, &mut tracking)
            .await
            .expect("first refill compacts");
        assert!(r1.was_compacted);
        assert_eq!(r1.consecutive_rapid_refills, 1);
        assert_eq!(tracking.consecutive_rapid_refills, 1);

        // Second refill still within window (turn_counter reset to 0 by the
        // first compact): kho = 1 + 1 = 2 → still below breaker → compacts,
        // carries 2.
        let r2 = orch
            .process_iteration_tracked(msgs, 0, &mut tracking)
            .await
            .expect("second refill compacts");
        assert!(r2.was_compacted);
        assert_eq!(r2.consecutive_rapid_refills, 2);
        assert!(!r2.rapid_refill_breaker_tripped);
    }
}
