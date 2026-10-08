//! Prompt-cache ledger — the per-session record behind `/cost`'s
//! `Prompt cache (main):` line and the `prompt_cache` status field (CLI-4).
//!
//! Oracle: `services/api/promptCacheLedger.ts`, class `nn` in the 2.1.286
//! binary, with `Ts` as the per-agent map and `Z$e`/`xKn` as the read seams.
//!
//! The ledger answers one question the raw token counters cannot: WHY the
//! cached prefix stopped being reused. Every request lands as an entry; the
//! entry's outcome is derived by comparing this request's token split against
//! the previous one, and a caller that KNOWS it just changed the prefix
//! ([`Self::attribute`]) or deliberately rebuilt it ([`Self::expect_drop`])
//! says so first, so a compaction is not reported as a cache fault.
//!
//! ⚠️ Every threshold here is a heuristic over token counts the provider
//! reports. It diagnoses the LIKELY cause; it cannot observe the server's
//! cache. `LikelyServerSide` exists precisely for "the prompt did not change
//! and it missed anyway".

use std::collections::HashMap;

/// `aW = 300000` / `BW = 3600000` — how long a written prefix stays warm.
const TTL_5M_MS: u64 = 300_000;
const TTL_1H_MS: u64 = 3_600_000;

/// `ZRo = 2000` — how many tokens must FAIL to come from cache before a
/// shrunken read counts as a miss rather than noise. Below it, a prefix that
/// grew by a few tokens would register as a fault every turn.
const MISS_TOKEN_THRESHOLD: u64 = 2_000;

/// `JRo = 200` — retained entries. The ledger is a rolling window: the
/// summary's counters are cumulative, only the per-request history is capped.
const MAX_ENTRIES: usize = 200;

/// `0.95` — a read covering at least 95% of the comparable prefix is a hit.
/// Providers round and re-block, so exact equality never holds.
const HIT_COVERAGE: f64 = 0.95;

/// The TTL a request wrote its cache block under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTtl {
    /// `"5m"`.
    FiveMinutes,
    /// `"1h"`.
    OneHour,
}

impl CacheTtl {
    /// `Dyn[ttl]` — the warm window in milliseconds.
    #[must_use]
    pub fn window_ms(self) -> u64 {
        match self {
            Self::FiveMinutes => TTL_5M_MS,
            Self::OneHour => TTL_1H_MS,
        }
    }

    /// The wire spelling `/cost` and the status field print.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }
}

/// What the ledger concluded about one request's use of the cached prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    /// First request, or the first one this session to report cache tokens.
    Cold,
    /// This provider reports no cache tokens at all — caching is off or
    /// unsupported. Distinct from a miss: nothing was expected to be cached.
    Uncached,
    /// The cached prefix was reused.
    Hit,
    /// The prefix shrank materially with no compaction to explain it.
    Miss,
    /// The prefix shrank, and a caller had announced the rebuild in advance.
    ExpectedRebuild,
}

/// The closed set of miss causes (`PROMPT_CACHE_MISS_CAUSES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MissCause {
    /// `system_prompt_changed`
    SystemPromptChanged,
    /// `tools_changed`
    ToolsChanged,
    /// `model_changed`
    ModelChanged,
    /// `fast_mode_changed`
    FastModeChanged,
    /// `cache_scope_or_ttl_changed`
    CacheScopeOrTtlChanged,
    /// `betas_changed`
    BetasChanged,
    /// `effort_changed`
    EffortChanged,
    /// `thinking_mode_changed`
    ThinkingModeChanged,
    /// `thinking_display_changed`
    ThinkingDisplayChanged,
    /// `auto_mode_changed`
    AutoModeChanged,
    /// `overage_changed`
    OverageChanged,
    /// `extra_body_changed`
    ExtraBodyChanged,
    /// `defer_loading_changed`
    DeferLoadingChanged,
    /// `messages_rewritten`
    MessagesRewritten,
    /// `ttl_expired_5m`
    TtlExpired5m,
    /// `ttl_expired_1h`
    TtlExpired1h,
    /// `likely_server_side` — the prompt did not change and it missed anyway.
    LikelySeverSide,
    /// `unknown`
    Unknown,
}

impl MissCause {
    /// The wire name (`miss_causes` keys, `last_miss_cause.causes` entries).
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::SystemPromptChanged => "system_prompt_changed",
            Self::ToolsChanged => "tools_changed",
            Self::ModelChanged => "model_changed",
            Self::FastModeChanged => "fast_mode_changed",
            Self::CacheScopeOrTtlChanged => "cache_scope_or_ttl_changed",
            Self::BetasChanged => "betas_changed",
            Self::EffortChanged => "effort_changed",
            Self::ThinkingModeChanged => "thinking_mode_changed",
            Self::ThinkingDisplayChanged => "thinking_display_changed",
            Self::AutoModeChanged => "auto_mode_changed",
            Self::OverageChanged => "overage_changed",
            Self::ExtraBodyChanged => "extra_body_changed",
            Self::DeferLoadingChanged => "defer_loading_changed",
            Self::MessagesRewritten => "messages_rewritten",
            Self::TtlExpired5m => "ttl_expired_5m",
            Self::TtlExpired1h => "ttl_expired_1h",
            Self::LikelySeverSide => "likely_server_side",
            Self::Unknown => "unknown",
        }
    }

    /// The human label `/cost` prints — oracle `Nyn`, byte-exact.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::SystemPromptChanged => "system prompt changed",
            Self::ToolsChanged => "tool definitions changed",
            Self::ModelChanged => "model changed",
            Self::FastModeChanged => "fast mode toggled",
            Self::CacheScopeOrTtlChanged => "cache scope or TTL changed",
            Self::BetasChanged => "beta headers changed",
            Self::EffortChanged => "effort changed",
            Self::ThinkingModeChanged => "thinking toggled",
            Self::ThinkingDisplayChanged => "thinking display changed",
            Self::AutoModeChanged => "auto mode toggled",
            Self::OverageChanged => "usage-limit state changed",
            Self::ExtraBodyChanged => "extra request fields changed",
            Self::DeferLoadingChanged => "deferred tool loading changed",
            Self::MessagesRewritten => "earlier messages changed",
            Self::TtlExpired5m => "idle past the 5m TTL",
            Self::TtlExpired1h => "idle past the 1h TTL",
            // U+2014 em dash, as upstream.
            Self::LikelySeverSide => "prompt unchanged \u{2014} likely server-side",
            Self::Unknown => "unknown",
        }
    }
}

/// Why a caller believes the prefix changed, supplied BEFORE the request whose
/// outcome it explains.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MissAttribution {
    /// One or more causes; rendered in order.
    pub causes: Vec<MissCause>,
    /// Accompanies [`MissCause::ToolsChanged`].
    pub tools_added: Option<u32>,
    /// Accompanies [`MissCause::ToolsChanged`].
    pub tools_removed: Option<u32>,
    /// Accompanies [`MissCause::SystemPromptChanged`]; signed.
    pub system_char_delta: Option<i64>,
}

/// One request's token split, as the provider reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestFacts {
    /// Unix epoch milliseconds.
    pub at_ms: u64,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// `cache_read_input_tokens`.
    pub cache_read_tokens: u64,
    /// `cache_creation_input_tokens`.
    pub cache_creation_tokens: u64,
    /// The TTL this request asked for.
    pub ttl: CacheTtl,
}

/// A recorded request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntry {
    /// The facts as recorded, except `ttl` — see [`PromptCacheLedger::record`].
    pub facts: RequestFacts,
    /// What the ledger concluded.
    pub outcome: CacheOutcome,
    /// Present only on a [`CacheOutcome::Miss`] a caller explained.
    pub attribution: Option<MissAttribution>,
}

/// The read shape `/cost` and the status field consume (`ROe`).
#[derive(Debug, Clone, PartialEq)]
pub struct CacheSummary {
    /// Requests recorded this session.
    pub requests: u64,
    /// Requests that reused the prefix.
    pub hits: u64,
    /// Requests whose prefix shrank with nothing to explain it.
    pub misses: u64,
    /// Rebuilds a caller announced (compaction, tool-result clearing).
    pub expected_rebuilds: u64,
    /// Requests that started a fresh prefix.
    pub cold_starts: u64,
    /// `cache_read / (cache_read + cache_creation + uncached input)`; `None`
    /// before any tokens are recorded.
    pub hit_ratio: Option<f64>,
    /// All `cache_creation` tokens this session.
    pub cache_write_tokens: u64,
    /// `cache_creation` tokens written by the requests counted as misses.
    pub miss_recache_tokens: u64,
    /// Epoch ms of the last miss.
    pub last_miss_at: Option<u64>,
    /// The last miss's diagnosis.
    pub last_miss_attribution: Option<MissAttribution>,
    /// Misses per cause this session.
    pub miss_causes: HashMap<MissCause, u64>,
    /// The most recent entry.
    pub last_request: Option<LedgerEntry>,
    /// Any response reported cache tokens.
    pub caching_observed: bool,
    /// Epoch ms of the last request or [`PromptCacheLedger::touch`].
    pub last_activity_at: Option<u64>,
    /// Epoch ms the prefix goes cold; `None` when the last response reported
    /// no cache tokens.
    pub expires_at: Option<u64>,
    /// The prefix is still inside its TTL right now.
    pub warm: bool,
}

/// Per-session prompt-cache ledger (oracle `fat`).
#[derive(Debug, Default)]
pub struct PromptCacheLedger {
    entries: Vec<LedgerEntry>,
    requests: u64,
    hits: u64,
    misses: u64,
    expected_rebuilds: u64,
    cold_starts: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    input_tokens: u64,
    miss_recache_tokens: u64,
    last_miss_at: Option<u64>,
    last_miss_attribution: Option<MissAttribution>,
    miss_causes: HashMap<MissCause, u64>,
    pending_attribution: Option<MissAttribution>,
    drop_expected_at: Option<u64>,
    touched_at: Option<u64>,
}

impl PromptCacheLedger {
    /// A fresh ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark activity that did NOT go through the API but still refreshed the
    /// server-side prefix's idle clock (`touch`).
    ///
    /// No-op before the first request: there is no prefix to keep warm, and
    /// recording one would make a session look warm before it ever called out.
    pub fn touch(&mut self, at_ms: u64) {
        if !self.entries.is_empty() {
            self.touched_at = Some(self.touched_at.map_or(at_ms, |t| t.max(at_ms)));
        }
    }

    /// Announce that the NEXT request deliberately rebuilds the prefix — a
    /// compaction or a tool-result clearing (`expectDrop`).
    ///
    /// This is what keeps a compaction out of the miss count. It is consumed by
    /// the next [`Self::record`] whether or not a drop actually happens.
    pub fn expect_drop(&mut self, at_ms: u64) {
        self.drop_expected_at = Some(at_ms);
    }

    /// Supply the diagnosis for the NEXT request, if it turns out to be a miss
    /// (`attribute`). Consumed by the next [`Self::record`].
    pub fn attribute(&mut self, attribution: MissAttribution) {
        self.pending_attribution = Some(attribution);
    }

    /// Record one request and return its entry (`record`).
    pub fn record(&mut self, facts: RequestFacts) -> LedgerEntry {
        let previous = self.entries.last().cloned();
        let drop_expected_at = self.drop_expected_at.take();
        let pending = self.pending_attribution.take();

        // A rebuild only counts as EXPECTED while the announced prefix could
        // still have been warm. Past its TTL the prefix was gone anyway, so the
        // shrink is not the compaction's doing.
        let drop_within_ttl = match (drop_expected_at, previous.as_ref()) {
            (Some(_), Some(prev)) => {
                let last_activity = prev.facts.at_ms.max(self.touched_at.unwrap_or(0));
                facts.at_ms.saturating_sub(last_activity) < prev.facts.ttl.window_ms()
            }
            _ => false,
        };

        let caching_observed = self.cache_read_tokens + self.cache_creation_tokens > 0
            || facts.cache_read_tokens + facts.cache_creation_tokens > 0;

        let outcome = match previous.as_ref() {
            None => CacheOutcome::Cold,
            Some(_) if !caching_observed => CacheOutcome::Uncached,
            Some(prev)
                if prev.facts.cache_read_tokens + prev.facts.cache_creation_tokens == 0
                    && facts.cache_read_tokens + facts.cache_creation_tokens > 0 =>
            {
                CacheOutcome::Cold
            }
            Some(prev) => {
                let prev_total = prev.facts.input_tokens
                    + prev.facts.cache_read_tokens
                    + prev.facts.cache_creation_tokens;
                let now_total =
                    facts.input_tokens + facts.cache_read_tokens + facts.cache_creation_tokens;
                // Only the OVERLAP can be compared: a prompt that grew cannot
                // be faulted for the part that did not exist last time.
                let comparable = prev_total.min(now_total);
                let unread = comparable.saturating_sub(facts.cache_read_tokens);
                #[allow(clippy::cast_precision_loss)]
                let covered =
                    (facts.cache_read_tokens as f64) >= (comparable as f64) * HIT_COVERAGE;
                if covered || unread < MISS_TOKEN_THRESHOLD {
                    CacheOutcome::Hit
                } else if drop_within_ttl {
                    CacheOutcome::ExpectedRebuild
                } else {
                    CacheOutcome::Miss
                }
            }
        };

        // A request that wrote NO cache block inherits the previous entry's
        // TTL: the prefix it read is still living under the TTL that created
        // it, and reporting this request's requested TTL would say the block
        // expires at a time it does not.
        let ttl = if facts.cache_creation_tokens == 0 {
            previous.as_ref().map_or(facts.ttl, |p| p.facts.ttl)
        } else {
            facts.ttl
        };

        let entry = LedgerEntry {
            facts: RequestFacts { ttl, ..facts },
            outcome,
            attribution: if outcome == CacheOutcome::Miss {
                pending.clone()
            } else {
                None
            },
        };
        self.entries.push(entry.clone());
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }

        self.requests += 1;
        self.cache_read_tokens += facts.cache_read_tokens;
        self.cache_creation_tokens += facts.cache_creation_tokens;
        self.input_tokens += facts.input_tokens;
        match outcome {
            CacheOutcome::Hit => self.hits += 1,
            CacheOutcome::Miss => {
                self.misses += 1;
                self.miss_recache_tokens += facts.cache_creation_tokens;
                self.last_miss_at = Some(facts.at_ms);
                self.last_miss_attribution = pending.clone();
                for cause in pending.iter().flat_map(|a| a.causes.iter()) {
                    *self.miss_causes.entry(*cause).or_insert(0) += 1;
                }
            }
            CacheOutcome::ExpectedRebuild => self.expected_rebuilds += 1,
            CacheOutcome::Cold => self.cold_starts += 1,
            CacheOutcome::Uncached => {}
        }
        entry
    }

    /// The read shape (`summary`). `now_ms` decides only [`CacheSummary::warm`].
    #[must_use]
    pub fn summary(&self, now_ms: u64) -> CacheSummary {
        let total = self.cache_read_tokens + self.cache_creation_tokens + self.input_tokens;
        let last = self.entries.last().cloned();
        let window = last
            .as_ref()
            .map_or(CacheTtl::FiveMinutes, |e| e.facts.ttl)
            .window_ms();
        let caching_observed = self.cache_read_tokens + self.cache_creation_tokens > 0;
        let last_wrote_or_read = last
            .as_ref()
            .is_some_and(|e| e.facts.cache_read_tokens + e.facts.cache_creation_tokens > 0);
        let last_activity = last
            .as_ref()
            .map(|e| e.facts.at_ms.max(self.touched_at.unwrap_or(0)));
        #[allow(clippy::cast_precision_loss)]
        let hit_ratio = (total > 0).then(|| self.cache_read_tokens as f64 / total as f64);
        CacheSummary {
            requests: self.requests,
            hits: self.hits,
            misses: self.misses,
            expected_rebuilds: self.expected_rebuilds,
            cold_starts: self.cold_starts,
            hit_ratio,
            cache_write_tokens: self.cache_creation_tokens,
            miss_recache_tokens: self.miss_recache_tokens,
            last_miss_at: self.last_miss_at,
            last_miss_attribution: self.last_miss_attribution.clone(),
            miss_causes: self.miss_causes.clone(),
            caching_observed,
            expires_at: last_activity
                .filter(|_| last_wrote_or_read)
                .map(|a| a + window),
            warm: last_wrote_or_read
                && last_activity.is_some_and(|a| now_ms.saturating_sub(a) < window),
            last_activity_at: last_activity.filter(|_| last.is_some()),
            last_request: last,
        }
    }

    /// Tokens the next request re-caches if the prefix is cold by then
    /// (`estimateRecacheTokens`).
    ///
    /// `Some(0)` before any request. `None` while a rebuild is ANNOUNCED but
    /// not yet recorded: the next prompt is about to be replaced, so the
    /// current one's size predicts nothing — which is why `/cost` prints
    /// "next turn re-caches the compacted prompt" rather than a number.
    #[must_use]
    pub fn estimate_recache_tokens(&self) -> Option<u64> {
        let Some(last) = self.entries.last() else {
            return Some(0);
        };
        if self.drop_expected_at.is_some() {
            return None;
        }
        Some(
            last.facts.input_tokens
                + last.facts.cache_read_tokens
                + last.facts.cache_creation_tokens,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(at_ms: u64, input: u64, read: u64, creation: u64) -> RequestFacts {
        RequestFacts {
            at_ms,
            input_tokens: input,
            cache_read_tokens: read,
            cache_creation_tokens: creation,
            ttl: CacheTtl::FiveMinutes,
        }
    }

    #[test]
    fn the_first_request_is_cold_and_a_reused_prefix_is_a_hit() {
        let mut l = PromptCacheLedger::new();
        assert_eq!(
            l.record(facts(0, 100, 0, 10_000)).outcome,
            CacheOutcome::Cold
        );
        // Reads back essentially the whole prefix ⇒ hit.
        assert_eq!(
            l.record(facts(1_000, 50, 10_000, 0)).outcome,
            CacheOutcome::Hit
        );
        let s = l.summary(2_000);
        assert_eq!((s.requests, s.hits, s.misses, s.cold_starts), (2, 1, 0, 1));
        assert!(s.warm, "1s after a 5m-TTL write the prefix is warm");
    }

    #[test]
    fn a_provider_that_reports_no_cache_tokens_is_uncached_not_missing() {
        // The distinction that keeps a non-caching provider from reading as a
        // session of constant cache faults.
        let mut l = PromptCacheLedger::new();
        l.record(facts(0, 5_000, 0, 0));
        assert_eq!(
            l.record(facts(1_000, 9_000, 0, 0)).outcome,
            CacheOutcome::Uncached
        );
        let s = l.summary(2_000);
        assert_eq!(s.misses, 0);
        assert!(!s.caching_observed);
        assert!(!s.warm);
    }

    #[test]
    fn a_prefix_that_shrinks_without_explanation_is_a_miss() {
        let mut l = PromptCacheLedger::new();
        l.record(facts(0, 100, 0, 50_000));
        // Reads back almost nothing of a 50k prefix.
        let e = l.record(facts(1_000, 100, 1_000, 49_000));
        assert_eq!(e.outcome, CacheOutcome::Miss);
        let s = l.summary(2_000);
        assert_eq!(s.misses, 1);
        assert_eq!(
            s.miss_recache_tokens, 49_000,
            "a miss's re-cache cost is its cache_creation tokens"
        );
        assert_eq!(s.last_miss_at, Some(1_000));
    }

    #[test]
    fn an_announced_rebuild_is_not_counted_as_a_fault() {
        // The whole reason `expect_drop` exists: a compaction rewrites the
        // prefix ON PURPOSE, and charging it as a cache fault would make every
        // compaction look like a bug.
        let mut l = PromptCacheLedger::new();
        l.record(facts(0, 100, 0, 50_000));
        l.expect_drop(900);
        let e = l.record(facts(1_000, 100, 1_000, 49_000));
        assert_eq!(e.outcome, CacheOutcome::ExpectedRebuild);
        let s = l.summary(2_000);
        assert_eq!((s.misses, s.expected_rebuilds), (0, 1));
    }

    #[test]
    fn an_announcement_past_the_ttl_no_longer_excuses_the_drop() {
        // Past its TTL the prefix was gone anyway, so the shrink is not the
        // compaction's doing and the miss is real.
        let mut l = PromptCacheLedger::new();
        l.record(facts(0, 100, 0, 50_000));
        l.expect_drop(1);
        let e = l.record(facts(TTL_5M_MS + 1, 100, 1_000, 49_000));
        assert_eq!(e.outcome, CacheOutcome::Miss);
    }

    #[test]
    fn a_small_shrink_is_noise_not_a_miss() {
        // Under `MISS_TOKEN_THRESHOLD` a prefix that grew by a little would
        // otherwise register a fault every single turn.
        let mut l = PromptCacheLedger::new();
        l.record(facts(0, 100, 0, 50_000));
        let e = l.record(facts(1_000, 1_500, 49_500, 500));
        assert_eq!(e.outcome, CacheOutcome::Hit);
    }

    #[test]
    fn a_miss_keeps_its_attribution_and_counts_it() {
        let mut l = PromptCacheLedger::new();
        l.record(facts(0, 100, 0, 50_000));
        l.attribute(MissAttribution {
            causes: vec![MissCause::ToolsChanged],
            tools_added: Some(2),
            tools_removed: Some(0),
            system_char_delta: None,
        });
        let e = l.record(facts(1_000, 100, 1_000, 49_000));
        assert_eq!(e.outcome, CacheOutcome::Miss);
        assert_eq!(
            e.attribution.as_ref().map(|a| a.causes.clone()),
            Some(vec![MissCause::ToolsChanged])
        );
        let s = l.summary(2_000);
        assert_eq!(s.miss_causes.get(&MissCause::ToolsChanged), Some(&1));
    }

    #[test]
    fn an_attribution_is_consumed_even_when_the_request_hits() {
        // Otherwise a stale diagnosis would be pinned to a LATER, unrelated
        // miss — the wrong cause is worse than none.
        let mut l = PromptCacheLedger::new();
        l.record(facts(0, 100, 0, 50_000));
        l.attribute(MissAttribution {
            causes: vec![MissCause::ModelChanged],
            ..MissAttribution::default()
        });
        assert_eq!(
            l.record(facts(1_000, 50, 50_000, 0)).outcome,
            CacheOutcome::Hit
        );
        let e = l.record(facts(2_000, 100, 1_000, 49_000));
        assert_eq!(e.outcome, CacheOutcome::Miss);
        assert_eq!(e.attribution, None, "the diagnosis belonged to the hit");
    }

    #[test]
    fn a_request_that_writes_nothing_inherits_the_previous_ttl() {
        // Reporting this request's REQUESTED ttl would say the block expires at
        // a time it does not: the prefix still lives under the TTL that wrote it.
        let mut l = PromptCacheLedger::new();
        l.record(RequestFacts {
            ttl: CacheTtl::OneHour,
            ..facts(0, 100, 0, 50_000)
        });
        let e = l.record(RequestFacts {
            ttl: CacheTtl::FiveMinutes,
            ..facts(1_000, 50, 50_000, 0)
        });
        assert_eq!(e.facts.ttl, CacheTtl::OneHour);
        assert_eq!(l.summary(2_000).expires_at, Some(1_000 + TTL_1H_MS));
    }

    #[test]
    fn touch_extends_the_warm_window_but_only_after_a_request() {
        let mut l = PromptCacheLedger::new();
        // Before any request there is no prefix to keep warm.
        l.touch(10_000);
        assert_eq!(l.summary(10_000).last_activity_at, None);

        l.record(facts(0, 100, 0, 50_000));
        assert!(!l.summary(TTL_5M_MS + 1).warm, "gone cold on its own");
        l.touch(TTL_5M_MS - 1);
        assert!(
            l.summary(TTL_5M_MS + 1).warm,
            "a touch inside the window restarts the idle clock"
        );
    }

    #[test]
    fn an_announced_rebuild_makes_the_recache_estimate_unpredictable() {
        let mut l = PromptCacheLedger::new();
        assert_eq!(l.estimate_recache_tokens(), Some(0));
        l.record(facts(0, 100, 0, 50_000));
        assert_eq!(l.estimate_recache_tokens(), Some(50_100));
        l.expect_drop(1);
        assert_eq!(
            l.estimate_recache_tokens(),
            None,
            "the prompt is about to be replaced, so its size predicts nothing"
        );
    }

    #[test]
    fn the_entry_window_is_capped_while_the_counters_are_cumulative() {
        let mut l = PromptCacheLedger::new();
        for i in 0..(MAX_ENTRIES as u64 + 50) {
            l.record(facts(i, 10, 0, 0));
        }
        assert_eq!(l.entries.len(), MAX_ENTRIES);
        assert_eq!(
            l.summary(0).requests,
            MAX_ENTRIES as u64 + 50,
            "the rolling window must not truncate the counters"
        );
    }

    #[test]
    fn the_cause_names_and_labels_are_the_oracle_set() {
        assert_eq!(
            MissCause::SystemPromptChanged.wire(),
            "system_prompt_changed"
        );
        assert_eq!(MissCause::LikelySeverSide.wire(), "likely_server_side");
        assert_eq!(MissCause::ToolsChanged.label(), "tool definitions changed");
        assert_eq!(MissCause::TtlExpired1h.label(), "idle past the 1h TTL");
        assert_eq!(
            MissCause::ThinkingModeChanged.wire(),
            "thinking_mode_changed"
        );
        assert_eq!(MissCause::ThinkingModeChanged.label(), "thinking toggled");
        assert_eq!(
            MissCause::ThinkingDisplayChanged.wire(),
            "thinking_display_changed"
        );
        assert_eq!(
            MissCause::ThinkingDisplayChanged.label(),
            "thinking display changed"
        );
        // U+2014, not an ASCII hyphen.
        assert_eq!(
            MissCause::LikelySeverSide.label(),
            "prompt unchanged \u{2014} likely server-side"
        );
        assert_eq!(CacheTtl::FiveMinutes.as_str(), "5m");
        assert_eq!(CacheTtl::OneHour.window_ms(), 3_600_000);
    }
}
