//! Memdir scanner + fixed-point u64 ranker.
//!
//! Enumerates `~/.lingxi/memdir/` (+ optional `~/.lingxi/team-mem/`),
//! drops entries older than 365 days, and ranks survivors by
//! `jaccard × age_weight × tier_weight × team_boost` in bps.

pub mod age;
pub mod find;
pub mod paths;
pub mod scan;
pub mod team_paths;
pub mod team_prompts;

pub use age::age_weight_bps;
pub use find::{find_relevant, RelevanceInputs};
pub use paths::{memdir_path, MemdirRoots, MEMDIR_SUBDIR, TEAM_MEM_SUBDIR};
pub use scan::{scan_memdir, MemdirSnapshot};

use protocol::{MemoryEntry, MemoryEntryTier};
use std::sync::Arc;

/// Telemetry event name emitted once per memory-load operation.
pub const TENGU_AGENT_MEMORY_LOADED: &str = "tengu_agent_memory_loaded";

/// Emit `tengu_agent_memory_loaded` for a finished load pass.
///
/// Payload (locked):
///   - `count: Int(N)` — number of entries surfaced
///   - `sources: String("session,project,team,user")` — comma-joined tier
///     list in declaration order (Session, Project, Team, User), only
///     tiers contributing ≥1 entry
///   - `had_team_boost: Bool(b)` — mirrors `settings.team_memory.enabled`
///     at the time of the load
pub async fn emit_agent_memory_loaded(
    bus: Option<&Arc<telemetry::AnalyticsBus>>,
    entries: &[MemoryEntry],
    had_team_boost: bool,
) {
    let Some(bus) = bus else {
        return;
    };
    let mut tiers_present = std::collections::HashSet::new();
    for e in entries {
        tiers_present.insert(e.tier);
    }
    // Emit in declaration order: Session, Project, Team, User.
    let mut parts = Vec::new();
    for t in [
        MemoryEntryTier::Session,
        MemoryEntryTier::Project,
        MemoryEntryTier::Team,
        MemoryEntryTier::User,
    ] {
        if tiers_present.contains(&t) {
            parts.push(match t {
                MemoryEntryTier::Session => "session",
                MemoryEntryTier::Project => "project",
                MemoryEntryTier::Team => "team",
                MemoryEntryTier::User => "user",
            });
        }
    }
    let mut md = telemetry::sink::LogEventMetadata::new();
    // Memory loads of >i64::MAX entries are unreachable in practice;
    // saturate to keep the event well-formed in any pathological case.
    let count = i64::try_from(entries.len()).unwrap_or(i64::MAX);
    md.insert("count".into(), telemetry::sink::AnalyticsValue::Int(count));
    md.insert(
        "sources".into(),
        telemetry::sink::AnalyticsValue::String(
            telemetry::pii::Verified::assert_safe(parts.join(",")).into_inner(),
        ),
    );
    md.insert(
        "had_team_boost".into(),
        telemetry::sink::AnalyticsValue::Bool(had_team_boost),
    );
    bus.log_event(TENGU_AGENT_MEMORY_LOADED, md).await;
}

#[cfg(test)]
mod telemetry_tests {
    use super::*;
    use protocol::{MemoryEntry, MemoryEntryTier};
    use std::sync::{Arc, Mutex};
    use telemetry::{sink::LogEventMetadata, AnalyticsBus, AnalyticsSink, AnalyticsValue};

    struct Cap {
        events: Mutex<Vec<(String, LogEventMetadata)>>,
    }
    #[async_trait::async_trait]
    impl AnalyticsSink for Cap {
        async fn log_event(&self, n: &str, m: LogEventMetadata) {
            self.events.lock().unwrap().push((n.into(), m));
        }
        async fn log_event_async(&self, n: &str, m: LogEventMetadata) {
            self.log_event(n, m).await;
        }
        fn name(&self) -> &str {
            "cap"
        }
    }

    fn e(tier: MemoryEntryTier, path: &str) -> MemoryEntry {
        MemoryEntry {
            path: path.into(),
            tier,
            body: "x".into(),
            age_days: 0,
            size_bytes: 1,
        }
    }

    #[tokio::test]
    async fn loaded_event_lists_tiers_present_in_declaration_order() {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(Cap {
            events: Mutex::new(Vec::new()),
        });
        bus.attach_sink(sink.clone()).await;
        let entries = vec![
            e(MemoryEntryTier::Project, "/p/a.md"),
            e(MemoryEntryTier::Session, "/s/a.md"),
            e(MemoryEntryTier::User, "/u/a.md"),
        ];
        emit_agent_memory_loaded(Some(&bus), &entries, true).await;
        let ev = sink.events.lock().unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].0, "tengu_agent_memory_loaded");
        let md = &ev[0].1;
        match md.get("count") {
            Some(AnalyticsValue::Int(3)) => {}
            other => panic!("count must be 3, got {other:?}"),
        }
        match md.get("sources") {
            Some(AnalyticsValue::String(s)) => assert_eq!(s, "session,project,user"),
            other => panic!("sources must be comma-joined tier list, got {other:?}"),
        }
        match md.get("had_team_boost") {
            Some(AnalyticsValue::Bool(true)) => {}
            other => panic!("had_team_boost must be true, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn loaded_event_empty_when_no_entries() {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(Cap {
            events: Mutex::new(Vec::new()),
        });
        bus.attach_sink(sink.clone()).await;
        emit_agent_memory_loaded(Some(&bus), &[], false).await;
        let ev = sink.events.lock().unwrap();
        assert_eq!(ev.len(), 1);
        match &ev[0].1.get("count") {
            Some(AnalyticsValue::Int(0)) => {}
            other => panic!("count must be 0, got {other:?}"),
        }
        match &ev[0].1.get("sources") {
            Some(AnalyticsValue::String(s)) => assert_eq!(s, ""),
            other => panic!("sources must be empty string, got {other:?}"),
        }
    }
}
