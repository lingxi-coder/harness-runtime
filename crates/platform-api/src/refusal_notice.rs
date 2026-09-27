//! Refusal-fallback notices: the episode state that accumulates them across
//! cascade hops, and the queue that collapses the ones a later hop supersedes
//! (claude-code `Jad`/`Xad`/`Oks` @232961500 and `j0m` @246308542, 2.1.220).
//!
//! A single-hop fallback can emit its notice immediately — there is nothing
//! that could withdraw it. A CASCADE cannot: hop 1's "switched to X" is only
//! true until hop 2 happens, at which point telling the user about X would be
//! telling them about a model the session already left.
//!
//! So a cascading notice is held PROVISIONALLY. If a later hop supersedes it,
//! the held notice is dropped and counted; when the episode finally settles,
//! ONE notice is emitted describing where the session actually ended up, and
//! `suppressed_count` reports how many intermediate hops were folded into it.
//! That count is the whole subject of
//! `tengu_refusal_fallback_notice_collapsed`.

/// A refusal-fallback notice — the `model_refusal_fallback` frame's payload.
///
/// Carries a `uuid` because a later notice must be able to NAME it in
/// `retracted_message_uuids`; the port's previous formatted-string banner had
/// no identity, so nothing could retract it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefusalNotice {
    /// This notice's identity.
    pub uuid: String,
    /// The model the episode STARTED on — preserved across hops, so the user
    /// is told where they came from rather than which intermediate hop the
    /// merge happened to see last.
    pub origin_model: String,
    /// The model now serving. Overwritten by each hop, because only the latest
    /// one is true.
    pub serving_model: String,
    /// Notices this one supersedes.
    pub retracted_message_uuids: Vec<String>,
    /// The user message that was refused — first-writer-wins, like
    /// `origin_model`.
    pub refused_user_message_uuid: Option<String>,
    /// The API's refusal category for the LATEST hop.
    pub api_refusal_category: Option<String>,
    /// The LATEST hop's request id.
    pub request_id: Option<String>,
    /// Set when this notice has been handed out provisionally; the uuid the
    /// provisional copy went out under, so a later merge can retract it.
    pub provisional_flush_uuid: Option<String>,
}

/// Why a notice was emitted (claude's `emittedVia`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmittedVia {
    /// Emitted directly — nothing was held.
    Immediate,
    /// Emitted because it superseded held notices.
    Supersedes,
    /// The held notice was flushed because a new, unrelated notice arrived.
    EpisodeBoundary,
    /// The episode settled.
    Settled,
}

impl EmittedVia {
    /// The wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::Supersedes => "supersedes",
            Self::EpisodeBoundary => "episode_boundary",
            Self::Settled => "settled",
        }
    }
}

/// A notice ready to emit, with what it collapsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedNotice {
    /// The notice.
    pub banner: RefusalNotice,
    /// How many held notices were folded into this one. `> 0` is what fires
    /// `tengu_refusal_fallback_notice_collapsed`.
    pub suppressed_count: u32,
    /// Why it was emitted.
    pub emitted_via: EmittedVia,
}

/// The per-episode accumulator (claude's `pendingNotice` half of the episode
/// state).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefusalEpisode {
    /// Whether a refusal episode is in progress.
    pub in_episode: bool,
    /// The notice accumulated so far.
    pub pending: Option<RefusalNotice>,
}

impl RefusalEpisode {
    /// Fold a new hop's notice into the pending one (claude `Jad`).
    ///
    /// `origin_model` and `refused_user_message_uuid` are FIRST-writer-wins —
    /// they describe where the episode began, and a later hop must not
    /// overwrite them. Everything else is latest-wins, because only the latest
    /// hop is currently true.
    ///
    /// A previous provisional flush is folded into `retracted_message_uuids`,
    /// which is how the settled notice comes to name every intermediate hop it
    /// replaced.
    pub fn merge(&mut self, incoming: RefusalNotice) {
        let prev = self.pending.take();
        let mut retracted: Vec<String> = Vec::new();
        if let Some(p) = &prev {
            retracted.extend(p.retracted_message_uuids.iter().cloned());
            if let Some(flushed) = &p.provisional_flush_uuid {
                retracted.push(flushed.clone());
            }
        }
        retracted.extend(incoming.retracted_message_uuids.iter().cloned());
        self.in_episode = true;
        self.pending = Some(RefusalNotice {
            uuid: incoming.uuid,
            origin_model: prev
                .as_ref()
                .map_or(incoming.origin_model.clone(), |p| p.origin_model.clone()),
            serving_model: incoming.serving_model,
            retracted_message_uuids: retracted,
            refused_user_message_uuid: prev
                .as_ref()
                .and_then(|p| p.refused_user_message_uuid.clone())
                .or(incoming.refused_user_message_uuid),
            api_refusal_category: incoming.api_refusal_category,
            request_id: incoming.request_id,
            // Cleared: the merged notice has not been flushed yet.
            provisional_flush_uuid: None,
        });
    }

    /// Hand out the pending notice PROVISIONALLY, stamping the uuid it goes out
    /// under (claude `Xad`).
    ///
    /// Returns `None` when there is nothing pending, or when it has ALREADY
    /// been flushed — flushing twice would show the user the same notice twice
    /// and leave two uuids for a later merge to retract.
    pub fn take_provisional(&mut self, flush_uuid: &str) -> Option<RefusalNotice> {
        let pending = self.pending.as_mut()?;
        if pending.provisional_flush_uuid.is_some() {
            return None;
        }
        let out = pending.clone();
        pending.provisional_flush_uuid = Some(flush_uuid.to_string());
        Some(out)
    }

    /// Settle the episode (claude `Oks`): take the pending notice ONLY if it
    /// was never flushed provisionally.
    ///
    /// A notice already shown provisionally is not re-emitted — the user has
    /// seen it, and the collapse queue is what reconciles it.
    pub fn settle(&mut self) -> Option<RefusalNotice> {
        self.in_episode = false;
        let pending = self.pending.take()?;
        pending.provisional_flush_uuid.is_none().then_some(pending)
    }

    /// Drop the pending notice without emitting (claude `Qad`).
    pub fn clear_pending(&mut self) {
        self.pending = None;
    }
}

/// The one-slot collapse queue (claude `j0m`).
///
/// Holds at most one PROVISIONAL notice. An incoming notice that names the held
/// one in its `retracted_message_uuids` collapses it — the held notice is
/// dropped, counted, and its uuid remembered so it can be scrubbed out of any
/// later notice's retraction list (naming an already-collapsed notice would ask
/// a consumer to retract something it never saw).
#[derive(Debug, Default)]
pub struct NoticeQueue {
    held: Option<RefusalNotice>,
    retracted: std::collections::HashSet<String>,
    suppressed: u32,
}

impl NoticeQueue {
    /// A fresh queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop uuids the queue already collapsed from a notice's retraction list.
    fn scrub(&self, mut notice: RefusalNotice) -> RefusalNotice {
        if self.retracted.is_empty() {
            return notice;
        }
        notice
            .retracted_message_uuids
            .retain(|u| !self.retracted.contains(u));
        notice
    }

    /// Emit `notice`, attaching and resetting the suppressed counter.
    fn emit(&mut self, notice: RefusalNotice, via: EmittedVia) -> EmittedNotice {
        let out = EmittedNotice {
            banner: self.scrub(notice),
            suppressed_count: self.suppressed,
            emitted_via: via,
        };
        self.suppressed = 0;
        out
    }

    fn flush_held(&mut self, via: EmittedVia) -> Vec<EmittedNotice> {
        match self.held.take() {
            Some(h) => vec![self.emit(h, via)],
            None => Vec::new(),
        }
    }

    /// Offer a notice to the queue.
    ///
    /// A PROVISIONAL notice is held rather than emitted. A non-provisional one
    /// is emitted — as `Supersedes` when it collapsed something, else
    /// `Immediate`.
    pub fn accept(&mut self, notice: RefusalNotice, provisional: bool) -> Vec<EmittedNotice> {
        let mut out = Vec::new();
        if let Some(held) = self.held.take() {
            if notice.retracted_message_uuids.contains(&held.uuid) {
                // The incoming notice supersedes the held one: collapse it.
                self.retracted.insert(held.uuid);
                self.suppressed += 1;
            } else {
                // Unrelated — the held notice was never superseded, so it is
                // still true and must reach the user.
                self.held = Some(held);
                out.extend(self.flush_held(EmittedVia::EpisodeBoundary));
            }
        }
        if provisional {
            self.held = Some(notice);
        } else {
            let via = if self.suppressed > 0 {
                EmittedVia::Supersedes
            } else {
                EmittedVia::Immediate
            };
            out.push(self.emit(notice, via));
        }
        out
    }

    /// Collapse the held notice by uuid, without an incoming one to supersede
    /// it (claude `dropHeld`).
    pub fn drop_held(&mut self, uuid: &str) {
        if self.held.as_ref().is_some_and(|h| h.uuid == uuid) {
            let h = self.held.take().expect("checked");
            self.retracted.insert(h.uuid);
            self.suppressed += 1;
        }
    }

    /// Flush whatever is held (claude `settle`).
    pub fn settle(&mut self, via: EmittedVia) -> Vec<EmittedNotice> {
        self.flush_held(via)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(uuid: &str, serving: &str) -> RefusalNotice {
        RefusalNotice {
            uuid: uuid.into(),
            origin_model: "origin".into(),
            serving_model: serving.into(),
            ..RefusalNotice::default()
        }
    }

    // ── episode accumulation ────────────────────────────────────────────────

    /// `origin_model` describes where the EPISODE began, so a later hop must
    /// not overwrite it — otherwise the user is told they came from an
    /// intermediate hop they never chose.
    #[test]
    fn merging_keeps_the_origin_and_takes_the_latest_serving_model() {
        let mut ep = RefusalEpisode::default();
        ep.merge(RefusalNotice {
            uuid: "n1".into(),
            origin_model: "opus".into(),
            serving_model: "sonnet".into(),
            refused_user_message_uuid: Some("u1".into()),
            ..RefusalNotice::default()
        });
        ep.merge(RefusalNotice {
            uuid: "n2".into(),
            origin_model: "sonnet".into(),
            serving_model: "haiku".into(),
            refused_user_message_uuid: Some("u2".into()),
            ..RefusalNotice::default()
        });
        let p = ep.pending.clone().unwrap();
        assert_eq!(p.origin_model, "opus", "origin is first-writer-wins");
        assert_eq!(p.serving_model, "haiku", "serving is latest-wins");
        assert_eq!(
            p.refused_user_message_uuid.as_deref(),
            Some("u1"),
            "the refused message is the episode's, not the latest hop's"
        );
        assert!(ep.in_episode);
    }

    /// The mechanism that makes a settled notice name every hop it replaced: a
    /// previous PROVISIONAL flush is folded into the retraction list on merge.
    #[test]
    fn a_previous_provisional_flush_is_folded_into_the_retraction_list() {
        let mut ep = RefusalEpisode::default();
        ep.merge(notice("n1", "sonnet"));
        assert!(ep.take_provisional("flushed-1").is_some());
        ep.merge(notice("n2", "haiku"));

        let p = ep.pending.unwrap();
        assert_eq!(p.retracted_message_uuids, vec!["flushed-1".to_string()]);
        assert!(
            p.provisional_flush_uuid.is_none(),
            "the merged notice has not been flushed yet"
        );
    }

    /// Flushing twice would show the user the same notice twice and leave two
    /// uuids for a later merge to retract.
    #[test]
    fn a_notice_is_only_handed_out_provisionally_once() {
        let mut ep = RefusalEpisode::default();
        ep.merge(notice("n1", "sonnet"));
        assert!(ep.take_provisional("f1").is_some());
        assert!(ep.take_provisional("f2").is_none());
    }

    /// A notice already shown provisionally is NOT re-emitted at settle — the
    /// user has seen it; the queue is what reconciles it.
    #[test]
    fn settling_skips_a_notice_that_was_already_flushed() {
        let mut ep = RefusalEpisode::default();
        ep.merge(notice("n1", "sonnet"));
        ep.take_provisional("f1");
        assert!(ep.settle().is_none());
        assert!(!ep.in_episode);

        // …but an unflushed one settles normally.
        let mut ep = RefusalEpisode::default();
        ep.merge(notice("n1", "sonnet"));
        assert_eq!(ep.settle().map(|n| n.uuid), Some("n1".to_string()));
    }

    #[test]
    fn settling_an_empty_episode_yields_nothing() {
        let mut ep = RefusalEpisode::default();
        assert!(ep.settle().is_none());
    }

    // ── the collapse queue ──────────────────────────────────────────────────

    /// A non-provisional notice goes straight out; nothing was collapsed.
    #[test]
    fn a_non_provisional_notice_is_emitted_immediately() {
        let mut q = NoticeQueue::new();
        let out = q.accept(notice("n1", "sonnet"), false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].suppressed_count, 0);
        assert_eq!(out[0].emitted_via, EmittedVia::Immediate);
    }

    /// A provisional notice is HELD — the user is not told about a hop that a
    /// later hop may replace.
    #[test]
    fn a_provisional_notice_is_held_not_emitted() {
        let mut q = NoticeQueue::new();
        assert!(q.accept(notice("n1", "sonnet"), true).is_empty());
        let out = q.settle(EmittedVia::Settled);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].banner.uuid, "n1");
    }

    /// THE POINT OF THE WHOLE SUBSYSTEM: a later notice that supersedes the
    /// held one collapses it, and the emitted notice reports the count that
    /// fires `tengu_refusal_fallback_notice_collapsed`.
    #[test]
    fn a_superseding_notice_collapses_the_held_one_and_counts_it() {
        let mut q = NoticeQueue::new();
        assert!(q.accept(notice("n1", "sonnet"), true).is_empty());

        let mut n2 = notice("n2", "haiku");
        n2.retracted_message_uuids = vec!["n1".into()];
        let out = q.accept(n2, false);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].banner.uuid, "n2");
        assert_eq!(out[0].suppressed_count, 1, "hop 1 was folded in");
        assert_eq!(out[0].emitted_via, EmittedVia::Supersedes);
    }

    /// Two intermediate hops collapse into one notice reporting both.
    #[test]
    fn successive_hops_accumulate_the_suppressed_count() {
        let mut q = NoticeQueue::new();
        q.accept(notice("n1", "a"), true);

        let mut n2 = notice("n2", "b");
        n2.retracted_message_uuids = vec!["n1".into()];
        assert!(q.accept(n2, true).is_empty(), "still provisional");

        let mut n3 = notice("n3", "c");
        n3.retracted_message_uuids = vec!["n2".into()];
        let out = q.accept(n3, false);
        assert_eq!(out[0].suppressed_count, 2);
    }

    /// An UNRELATED notice does not collapse the held one — that notice was
    /// never superseded, so it is still true and must reach the user.
    #[test]
    fn an_unrelated_notice_flushes_the_held_one_at_the_episode_boundary() {
        let mut q = NoticeQueue::new();
        q.accept(notice("n1", "sonnet"), true);
        let out = q.accept(notice("unrelated", "haiku"), false);

        assert_eq!(out.len(), 2, "the held one is flushed, then the new one");
        assert_eq!(out[0].banner.uuid, "n1");
        assert_eq!(out[0].emitted_via, EmittedVia::EpisodeBoundary);
        assert_eq!(out[0].suppressed_count, 0);
        assert_eq!(out[1].banner.uuid, "unrelated");
        assert_eq!(out[1].emitted_via, EmittedVia::Immediate);
    }

    /// Every uuid this queue collapsed is SCRUBBED from the emitted notice's
    /// retraction list — including the one collapsed by THIS call, since the
    /// scrub runs after the collapse.
    ///
    /// That is the right semantic, not an off-by-one: a notice the queue held
    /// and dropped never reached a consumer, so asking a consumer to retract it
    /// would name something it never saw. The list that goes out should contain
    /// only uuids that were actually emitted.
    #[test]
    fn every_collapsed_uuid_is_scrubbed_from_the_emitted_notice() {
        let mut q = NoticeQueue::new();
        q.accept(notice("n1", "a"), true);
        let mut n2 = notice("n2", "b");
        n2.retracted_message_uuids = vec!["n1".into()];
        q.accept(n2, true);

        let mut n3 = notice("n3", "c");
        // Names the already-collapsed n1 AND n2, which this call collapses.
        n3.retracted_message_uuids = vec!["n1".into(), "n2".into()];
        let out = q.accept(n3, false);
        assert!(
            out[0].banner.retracted_message_uuids.is_empty(),
            "neither collapsed notice ever reached a consumer: {:?}",
            out[0].banner.retracted_message_uuids
        );
        assert_eq!(out[0].suppressed_count, 2);
    }

    /// A uuid the queue never held is NOT scrubbed — it names a notice that
    /// really was emitted, so a consumer does need to retract it.
    #[test]
    fn a_uuid_the_queue_never_held_survives_the_scrub() {
        let mut q = NoticeQueue::new();
        let mut n = notice("n2", "b");
        n.retracted_message_uuids = vec!["emitted-elsewhere".into()];
        let out = q.accept(n, false);
        assert_eq!(
            out[0].banner.retracted_message_uuids,
            vec!["emitted-elsewhere".to_string()]
        );
    }

    /// `drop_held` collapses without an incoming notice, and only matches the
    /// uuid actually held.
    #[test]
    fn drop_held_collapses_only_the_matching_uuid() {
        let mut q = NoticeQueue::new();
        q.accept(notice("n1", "a"), true);
        q.drop_held("someone-else");
        assert_eq!(
            q.settle(EmittedVia::Settled).len(),
            1,
            "a non-matching uuid leaves the held notice alone"
        );

        let mut q = NoticeQueue::new();
        q.accept(notice("n1", "a"), true);
        q.drop_held("n1");
        assert!(q.settle(EmittedVia::Settled).is_empty());
        let out = q.accept(notice("n2", "b"), false);
        assert_eq!(out[0].suppressed_count, 1, "the drop was counted");
    }

    /// The counter resets on emit, so a second episode does not inherit the
    /// first's count.
    #[test]
    fn the_suppressed_count_resets_after_each_emit() {
        let mut q = NoticeQueue::new();
        q.accept(notice("n1", "a"), true);
        let mut n2 = notice("n2", "b");
        n2.retracted_message_uuids = vec!["n1".into()];
        assert_eq!(q.accept(n2, false)[0].suppressed_count, 1);
        assert_eq!(q.accept(notice("n3", "c"), false)[0].suppressed_count, 0);
    }

    #[test]
    fn emitted_via_strings_are_byte_locked() {
        assert_eq!(EmittedVia::Immediate.as_str(), "immediate");
        assert_eq!(EmittedVia::Supersedes.as_str(), "supersedes");
        assert_eq!(EmittedVia::EpisodeBoundary.as_str(), "episode_boundary");
    }

    /// End-to-end: a three-hop cascade produces ONE notice, telling the user
    /// where they started and where they ended, and reporting the two hops in
    /// between.
    #[test]
    fn a_three_hop_cascade_collapses_to_one_notice() {
        let mut ep = RefusalEpisode::default();
        let mut q = NoticeQueue::new();

        for (i, (uuid, serving)) in [("n1", "sonnet"), ("n2", "haiku"), ("n3", "opus")]
            .iter()
            .enumerate()
        {
            ep.merge(RefusalNotice {
                uuid: (*uuid).into(),
                origin_model: "opus-original".into(),
                serving_model: (*serving).into(),
                ..RefusalNotice::default()
            });
            let last = i == 2;
            if last {
                let settled = ep.settle().expect("an unflushed notice settles");
                let out = q.accept(settled, false);
                assert_eq!(out.len(), 1);
                assert_eq!(out[0].banner.origin_model, "opus-original");
                assert_eq!(out[0].banner.serving_model, "opus");
                assert_eq!(
                    out[0].suppressed_count, 2,
                    "the two intermediate hops were folded in"
                );
            } else {
                let provisional = ep.take_provisional(uuid).expect("held");
                assert!(q.accept(provisional, true).is_empty());
            }
        }
    }
}
