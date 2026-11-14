//! Bounded fallback bodies. Observer ownership/index state lives separately.

use super::SessionAgentTranscriptRows;
use client::protocol::listings::SessionAgentMessageRowDto;
use std::collections::VecDeque;
use std::hash::Hasher;
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) const FALLBACK_BYTES: usize = 8 * 1024 * 1024;
type Key = (String, String);

// Publication metadata outlives body eviction/retirement. A bounded LRU miss
// gets a newer generation, so evicting metadata cannot roll a client back.
const PUBLICATION_ENTRIES: usize = 256;
static PUBLICATION_REVISION: AtomicU64 = AtomicU64::new(0);
struct Publication {
    key: Key,
    fingerprint: u64,
    revision: u64,
}

pub(super) struct Row {
    uuid: Box<str>,
    source: Box<[u8]>,
}

pub(in crate::mobile) struct Entry {
    key: Key,
    pub(super) rows: VecDeque<Row>,
    tombstones: Vec<Box<str>>,
    pub(super) next_message_index: u64,
    revision: u64,
    active: bool,
    accessed: u64,
    body_bytes: usize,
}

impl Entry {
    fn owned_bytes(&self) -> usize {
        self.key.0.capacity()
            + self.key.1.capacity()
            + self.rows.capacity() * std::mem::size_of::<Row>()
            + self.tombstones.capacity() * std::mem::size_of::<Box<str>>()
            + self.tombstones.iter().map(|uuid| uuid.len()).sum::<usize>()
            + self.body_bytes
    }
}

pub(in crate::mobile) struct Cache {
    entries: Vec<Entry>,
    clock: u64,
    budget: usize,
    publications: VecDeque<Publication>,
    capture_gates: Vec<(Key, std::sync::Weak<tokio::sync::Mutex<()>>)>,
}

impl Default for Cache {
    fn default() -> Self {
        Self::with_budget(FALLBACK_BYTES)
    }
}

impl Cache {
    pub(super) fn with_budget(budget: usize) -> Self {
        Self {
            entries: Vec::new(),
            clock: 0,
            budget,
            publications: VecDeque::new(),
            capture_gates: Vec::new(),
        }
    }

    /// Exact fallback-body allocations: table, row slots, keys, UUID boxes and
    /// serialized rows. Publication metadata has its own bounded entry count;
    /// capture gates retain keys only while their owners are in flight.
    pub(super) fn owned_bytes(&self) -> usize {
        self.entries.capacity() * std::mem::size_of::<Entry>()
            + self.entries.iter().map(Entry::owned_bytes).sum::<usize>()
    }

    fn entry(&mut self, key: Key) -> &mut Entry {
        self.clock = self.clock.saturating_add(1);
        let index = match self.entries.iter().position(|entry| entry.key == key) {
            Some(index) => index,
            None => {
                self.entries.push(Entry {
                    key,
                    rows: VecDeque::new(),
                    tombstones: Vec::new(),
                    next_message_index: 0,
                    revision: 0,
                    active: true,
                    accessed: 0,
                    body_bytes: 0,
                });
                self.entries.len() - 1
            }
        };
        let entry = &mut self.entries[index];
        entry.accessed = self.clock;
        entry
    }

    pub(super) fn activate(&mut self, key: Key, next_index: u64) {
        let entry = self.entry(key);
        entry.active = true;
        entry.next_message_index = entry.next_message_index.max(next_index);
        self.trim();
    }

    pub(super) fn record(&mut self, key: Key, row: &SessionAgentMessageRowDto, revision: u64) {
        let budget = self.budget;
        // Bound the temporary serialization too. A single huge live message
        // must not allocate a second huge body merely to discover it cannot fit.
        let mut encoded = BoundedRowWriter {
            source: Vec::new(),
            limit: budget.saturating_sub(row.message_uuid.len()),
        };
        let source = serde_json::to_writer(&mut encoded, row)
            .ok()
            .map(|()| encoded.source.into_boxed_slice());
        let entry = self.entry(key);
        entry.active = true;
        entry.next_message_index = entry
            .next_message_index
            .max(row.message_index.saturating_add(1));
        entry.revision = revision;
        // An oversized live row is still delivered in full, but never retained.
        if let Some(source) = source {
            let uuid = row.message_uuid.clone().into_boxed_str();
            entry
                .tombstones
                .retain(|deleted| deleted.as_ref() != uuid.as_ref());
            entry.body_bytes += source.len() + uuid.len();
            entry.rows.push_back(Row { uuid, source });
        }
        self.trim();
    }

    pub(super) fn tombstone(&mut self, key: &Key, uuid: &str, revision: u64) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| &entry.key == key) {
            if !entry
                .tombstones
                .iter()
                .any(|deleted| deleted.as_ref() == uuid)
            {
                entry.tombstones.push(uuid.into());
            }
            entry.rows.retain(|row| row.uuid.as_ref() != uuid);
            entry.rows.shrink_to_fit();
            entry.body_bytes = entry
                .rows
                .iter()
                .map(|row| row.uuid.len() + row.source.len())
                .sum();
            entry.revision = revision;
        }
        self.trim();
    }

    pub(super) fn get(&mut self, key: &Key) -> Option<SessionAgentTranscriptRows> {
        self.clock = self.clock.saturating_add(1);
        let entry = self.entries.iter_mut().find(|entry| &entry.key == key)?;
        entry.accessed = self.clock;
        Some(SessionAgentTranscriptRows {
            rows: entry
                .rows
                .iter()
                .map(|row| {
                    serde_json::from_slice(&row.source)
                        .expect("cached row was serialized from the same DTO")
                })
                .collect(),
            next_message_index: entry.next_message_index,
            revision: entry.revision,
        })
    }

    pub(super) fn covered_by(
        &self,
        key: &Key,
        rows: &[SessionAgentMessageRowDto],
        next: u64,
    ) -> bool {
        let persisted = rows
            .iter()
            .map(|row| row.message_uuid.as_str())
            .collect::<std::collections::HashSet<_>>();
        self.entries
            .iter()
            .find(|entry| &entry.key == key)
            .is_some_and(|entry| {
                next >= entry.next_message_index
                    && entry
                        .tombstones
                        .iter()
                        .all(|uuid| !persisted.contains(uuid.as_ref()))
                    && entry
                        .rows
                        .iter()
                        .all(|cached| persisted.contains(cached.uuid.as_ref()))
            })
    }

    pub(super) fn merge_persisted(
        &mut self,
        key: &Key,
        mut rows: Vec<SessionAgentMessageRowDto>,
        revision: u64,
        next: u64,
    ) -> SessionAgentTranscriptRows {
        if let Some(entry) = self.entries.iter().find(|entry| &entry.key == key) {
            rows.retain(|row| {
                !entry
                    .tombstones
                    .iter()
                    .any(|uuid| uuid.as_ref() == row.message_uuid)
            });
        }
        let mut next_message_index = next;
        if let Some(snapshot) = self.get(key) {
            next_message_index = next_message_index.max(snapshot.next_message_index);
            let mut seen = rows
                .iter()
                .map(|row| row.message_uuid.clone())
                .collect::<std::collections::HashSet<_>>();
            for row in snapshot.rows {
                if seen.insert(row.message_uuid.clone()) {
                    rows.push(row);
                }
            }
        }
        rows.sort_by_key(|row| row.message_index);
        let snapshot = SessionAgentTranscriptRows {
            rows,
            revision,
            next_message_index,
        };
        self.publish(key, snapshot)
    }

    pub(super) fn publish(
        &mut self,
        key: &Key,
        mut snapshot: SessionAgentTranscriptRows,
    ) -> SessionAgentTranscriptRows {
        struct Fingerprint(std::collections::hash_map::DefaultHasher);
        impl std::io::Write for Fingerprint {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.write(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut fingerprint = Fingerprint(Default::default());
        serde_json::to_writer(
            &mut fingerprint,
            &(&snapshot.rows, snapshot.next_message_index),
        )
        .expect("transcript DTO serializes");
        let fingerprint = fingerprint.0.finish();
        if let Some(index) = self.publications.iter().position(|row| &row.key == key) {
            let previous = self.publications.remove(index).expect("publication exists");
            if previous.fingerprint == fingerprint {
                snapshot.revision = previous.revision;
                self.publications.push_back(previous);
                return snapshot;
            }
        }
        // Keep generations across host owners in this process. Wall-clock
        // microseconds also keep a rebuilt native host ahead of prior reads;
        // the atomic handles same-tick publications and clock regressions.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        let floor = snapshot
            .revision
            .max(u64::try_from(now).unwrap_or(u64::MAX - 1));
        let previous = PUBLICATION_REVISION
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                Some(value.max(floor).saturating_add(1))
            })
            .expect("infallible revision update");
        snapshot.revision = previous.max(floor).saturating_add(1);
        self.publications.push_back(Publication {
            key: key.clone(),
            fingerprint,
            revision: snapshot.revision,
        });
        if self.publications.len() > PUBLICATION_ENTRIES {
            self.publications.pop_front();
        }
        snapshot
    }

    pub(super) fn finish(&mut self, key: &Key, durable: bool) {
        if let Some(index) = self.entries.iter().position(|entry| &entry.key == key) {
            if durable {
                self.entries.remove(index);
                self.entries.shrink_to_fit();
            } else {
                self.entries[index].active = false;
            }
        }
        self.trim();
    }

    fn trim(&mut self) {
        while self.owned_bytes() > self.budget {
            // Completed fallback entries are LRU. Active bindings/indexes are
            // pinned in the observer, while their fallback keeps only a bounded
            // recent suffix; no active body can bypass this hard budget.
            if let Some(index) = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| !entry.active)
                .min_by_key(|(_, entry)| entry.accessed)
                .map(|(index, _)| index)
            {
                self.entries.remove(index);
                self.entries.shrink_to_fit();
            } else if let Some(index) = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| !entry.rows.is_empty())
                .min_by_key(|(_, entry)| entry.accessed)
                .map(|(index, _)| index)
            {
                let entry = &mut self.entries[index];
                let row = entry.rows.pop_front().unwrap();
                entry.body_bytes -= row.uuid.len() + row.source.len();
                entry.rows.shrink_to_fit();
            } else if !self.entries.is_empty() {
                // Only cache metadata remains; authoritative ownership is not
                // stored here and cannot be retired by body eviction.
                let index = self
                    .entries
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, entry)| entry.accessed)
                    .unwrap()
                    .0;
                self.entries.remove(index);
                self.entries.shrink_to_fit();
            } else {
                break;
            }
        }
    }
}

/// Serialize one child's captures and observer updates without holding the
/// shared body-cache mutex over disk I/O or blocking unrelated children.
pub(super) async fn lock_capture(
    cache: &std::sync::Arc<tokio::sync::Mutex<Cache>>,
    key: &Key,
) -> tokio::sync::OwnedMutexGuard<()> {
    let gate = {
        let mut cache = cache.lock().await;
        cache
            .capture_gates
            .retain(|(_, gate)| gate.strong_count() > 0);
        if let Some(gate) = cache
            .capture_gates
            .iter()
            .find_map(|(owner, gate)| (owner == key).then(|| gate.upgrade()).flatten())
        {
            gate
        } else {
            let gate = std::sync::Arc::new(tokio::sync::Mutex::new(()));
            cache
                .capture_gates
                .push((key.clone(), std::sync::Arc::downgrade(&gate)));
            gate
        }
    };
    gate.lock_owned().await
}

struct BoundedRowWriter {
    source: Vec<u8>,
    limit: usize,
}

impl std::io::Write for BoundedRowWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.source.len()) {
            return Err(std::io::Error::other(
                "fallback row exceeds its byte budget",
            ));
        }
        // Grow geometrically within the cap, rather than reserving on every
        // code unit emitted by the serializer or crossing the cap on growth.
        let needed = self.source.len() + bytes.len();
        if needed > self.source.capacity() {
            let target = self
                .source
                .capacity()
                .max(256)
                .saturating_mul(2)
                .max(needed)
                .min(self.limit);
            self.source.reserve_exact(target - self.source.len());
        }
        self.source.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::ops::Index<&Key> for Cache {
    type Output = Entry;
    fn index(&self, key: &Key) -> &Self::Output {
        self.entries
            .iter()
            .find(|entry| &entry.key == key)
            .expect("cached agent")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn child_capture_gates_serialize_one_child_and_keep_other_children_parallel() {
        let cache = std::sync::Arc::new(tokio::sync::Mutex::new(Cache::with_budget(8192)));
        let key = key_for("owner", "child");
        let held = lock_capture(&cache, &key).await;
        let same = lock_capture(&cache, &key);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), same)
                .await
                .is_err()
        );
        for index in 0..300 {
            let other_key = key_for("owner", &index.to_string());
            let other = lock_capture(&cache, &other_key);
            drop(
                tokio::time::timeout(std::time::Duration::from_secs(1), other)
                    .await
                    .unwrap(),
            );
        }
        assert!(cache.lock().await.capture_gates.len() <= 2);
        drop(held);
        drop(lock_capture(&cache, &key).await);
    }

    #[test]
    fn recovered_publications_advance_on_changes_and_survive_catchup_retirement_and_eviction() {
        let mut cache = Cache::with_budget(8192);
        let key = key_for("owner", "child");
        let disk = row(0, 20);
        let suffix = row(1, 20);
        let before = cache.merge_persisted(&key, vec![disk.clone()], 1, 1);
        cache.record(key.clone(), &suffix, 1);
        let recovered = cache.merge_persisted(&key, vec![disk.clone()], 1, 1);
        assert!(recovered.revision > before.revision);
        assert_eq!(recovered.rows.len(), 2);
        assert_eq!(
            cache
                .merge_persisted(&key, vec![disk.clone()], 1, 1)
                .revision,
            recovered.revision
        );
        let durable = vec![disk.clone(), suffix.clone()];
        cache.finish(&key, true);
        assert_eq!(
            cache.merge_persisted(&key, durable.clone(), 2, 2).revision,
            recovered.revision
        );
        cache.record(key.clone(), &suffix, 1);
        cache.tombstone(&key, &suffix.message_uuid, 2);
        let removed = cache.merge_persisted(&key, durable, 2, 2);
        assert!(removed.revision > recovered.revision);
        cache.finish(&key, true);
        assert_eq!(
            cache
                .merge_persisted(&key, vec![disk.clone()], 1, 2)
                .revision,
            removed.revision
        );
        for index in 0..=PUBLICATION_ENTRIES {
            cache.merge_persisted(&key_for("owner", &index.to_string()), Vec::new(), 0, 0);
        }
        assert!(cache.publications.len() <= PUBLICATION_ENTRIES);
        assert!(cache.merge_persisted(&key, vec![disk], 1, 2).revision > removed.revision);
    }

    fn key_for(session: &str, agent: &str) -> Key {
        (session.to_owned(), agent.to_owned())
    }

    fn row(index: u64, body_size: usize) -> SessionAgentMessageRowDto {
        let message = lingxi_core::types::ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "x".repeat(body_size).into(),
        );
        SessionAgentMessageRowDto {
            message_index: index,
            message_uuid: message.id().as_uuid().to_string(),
            message: client::adapter::lowering::lower_conversation_message_with(
                &message,
                &mut Default::default(),
            ),
            api_error_json: None,
        }
    }

    #[test]
    fn persisted_prefix_merges_unpersisted_suffix_without_duplicates_or_tombstone_resurrection() {
        let mut cache = Cache::with_budget(8192);
        let key = key_for("owner", "child");
        let first = row(7, 30);
        let deleted = row(8, 30);
        let unpersisted = row(11, 30);
        cache.activate(key.clone(), 11);
        cache.record(key.clone(), &first, 1);
        cache.record(key.clone(), &deleted, 2);
        cache.record(key.clone(), &unpersisted, 3);
        cache.tombstone(&key, &deleted.message_uuid, 4);
        cache.finish(&key, false);
        let disk = vec![first.clone(), deleted.clone()];
        for _ in 0..2 {
            let merged = cache.merge_persisted(&key, disk.clone(), 100, 11);
            assert_eq!(
                merged
                    .rows
                    .iter()
                    .map(|row| row.message_index)
                    .collect::<Vec<_>>(),
                [7, 11]
            );
            assert_eq!(merged.next_message_index, 12);
            assert_ne!(merged.revision, 100);
            assert!(!cache.covered_by(&key, &disk, 12));
        }
        let other = cache.merge_persisted(&key_for("other", "child"), disk, 100, 11);
        assert_eq!(other.rows.len(), 2);
        assert!(other.revision >= 100);
        let complete = vec![first, unpersisted];
        assert!(cache.covered_by(&key, &complete, 12));
        cache.finish(&key, true);
        assert!(cache.get(&key).is_none());
        let durable = cache.merge_persisted(&key, complete, 101, 12);
        assert_eq!(durable.rows.len(), 2);
        assert!(durable.revision >= 101);
        assert!(cache.owned_bytes() <= 8192);
    }

    #[test]
    fn one_active_child_has_a_hard_body_budget_and_keeps_recent_order_and_highwater() {
        let mut cache = Cache::with_budget(4096);
        let key = key_for("a", "child");
        for index in 0..100 {
            cache.record(key.clone(), &row(index, 700), index + 1);
            assert!(cache.owned_bytes() <= 4096);
        }
        let snapshot = cache.get(&key).unwrap();
        assert!(!snapshot.rows.is_empty());
        assert!(snapshot.rows.len() < 100);
        assert!(snapshot
            .rows
            .windows(2)
            .all(|rows| rows[0].message_index < rows[1].message_index));
        assert_eq!(snapshot.rows.last().unwrap().message_index, 99);
        assert_eq!(snapshot.next_message_index, 100);
        cache.record(key.clone(), &row(100, 100_000), 101);
        assert!(cache.owned_bytes() <= 4096);
        let snapshot = cache.get(&key).unwrap();
        assert_eq!(snapshot.next_message_index, 101);
        assert_eq!(snapshot.revision, 101);
        assert!(!snapshot.rows.iter().any(|row| row.message_index == 100));
    }

    #[test]
    fn terminal_fallback_is_lru_with_session_isolation_and_durable_retirement() {
        let row = row(0, 1000);
        let budget = 2 * (serde_json::to_vec(&row).unwrap().len() + row.message_uuid.len()) + 1024;
        let mut cache = Cache::with_budget(budget);
        let a = key_for("a", "same-child");
        let b = key_for("b", "same-child");
        let c = key_for("c", "third-child");
        cache.record(a.clone(), &row, 1);
        cache.finish(&a, false);
        cache.record(b.clone(), &row, 1);
        cache.finish(&b, false);
        assert!(cache.get(&a).is_some());
        cache.record(c.clone(), &row, 1);
        assert!(cache.owned_bytes() <= budget);
        assert!(
            cache.get(&a).is_some(),
            "recent completed fallback survives"
        );
        assert!(
            cache.get(&b).is_none(),
            "least-recent terminal body is evicted first"
        );
        cache.finish(&a, true);
        assert!(cache.get(&a).is_none());
        assert!(cache.get(&c).is_some());
    }

    #[test]
    fn tombstones_keep_gaps_and_highwater_after_body_eviction_and_wake() {
        let mut cache = Cache::with_budget(4096);
        let key = key_for("a", "child");
        let first = row(4, 100);
        cache.record(key.clone(), &first, 1);
        cache.tombstone(&key, &first.message_uuid, 2);
        let snapshot = cache.get(&key).unwrap();
        assert!(snapshot.rows.is_empty());
        assert_eq!(snapshot.next_message_index, 5);
        assert_eq!(snapshot.revision, 2);
        cache.finish(&key, false);
        cache.activate(key.clone(), 5);
        let next = row(5, 200);
        cache.record(key.clone(), &next, 3);
        assert!(!cache.covered_by(&key, &[], 6));
        assert!(!cache.covered_by(&key, &[next.clone()], 5));
        assert!(cache.covered_by(&key, &[next], 6));
        assert_eq!(cache.get(&key).unwrap().rows[0].message_index, 5);
        for session in 0..100 {
            let key = key_for(&session.to_string(), "child");
            cache.record(key.clone(), &row(0, 1000), 1);
            cache.finish(&key, false);
            assert!(cache.owned_bytes() <= 4096);
        }
    }
}
