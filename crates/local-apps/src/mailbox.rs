//! The app-to-conversation mailbox.
//!
//! A running app posts small structured events here (`agent.post`); the
//! assistant reads them through the local-apps MCP `read_app_events` tool.
//! This is the ONE direction the bridge never had: the agent could already
//! query an app's data and drive its UI, but an app had no way to tell the
//! conversation anything.
//!
//! Its own per-app document with its own bounds, in the same shape
//! `permissions.json` established.
//!
//! Everything an app writes here is UNTRUSTED input. The mailbox stores it
//! verbatim and never interprets it; the framing that keeps it out of the
//! assistant's instruction channel lives at the MCP tool.

use crate::error::AppError;
use crate::manifest::AppLayout;
use crate::types::APPS_SCHEMA_VERSION;
use rooted_fs::AtomicWriteOptions;
use rooted_fs::FsError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Most events one app may retain. The oldest go first.
pub const MAX_MAILBOX_EVENTS: usize = 64;
/// Byte cap on one serialized event's body.
pub const MAX_MAILBOX_BODY_BYTES: usize = 16 * 1024;
/// Byte cap on the whole document, checked before it is written.
pub const MAX_MAILBOX_BYTES: u64 = 256 * 1024;
/// Longest topic an app may use.
const MAX_TOPIC_LEN: usize = 64;

/// One event an app posted for the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppMailboxEvent {
    /// Monotonic per-app sequence number.
    pub seq: u64,
    /// App-chosen topic, `^[a-z0-9][a-z0-9_.-]{0,63}$`.
    pub topic: String,
    /// Optional app-owned Agent session recipient. `None` keeps the legacy
    /// conversation mailbox semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// App-supplied JSON. Stored verbatim, never interpreted.
    pub body: serde_json::Value,
    /// Post time, epoch milliseconds.
    pub created_at_ms: u64,
}

/// One app's durable mailbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppMailbox {
    /// Persisted local-app schema version.
    pub schema_version: u32,
    /// Next sequence number to mint.
    pub next_seq: u64,
    /// Highest sequence number a `drain` has handed out.
    #[serde(default)]
    pub last_read_seq: u64,
    /// How many events the caps evicted before anyone read them.
    #[serde(default)]
    pub dropped_count: u64,
    /// Loss counters for session-targeted events, keyed by session id.
    #[serde(default)]
    pub session_dropped_count: BTreeMap<String, u64>,
    /// Independent read cursors for session-targeted event streams.
    #[serde(default)]
    pub session_read_seq: BTreeMap<String, u64>,
    /// Retained events, oldest first.
    #[serde(default)]
    pub events: Vec<AppMailboxEvent>,
}

impl Default for AppMailbox {
    fn default() -> Self {
        Self {
            schema_version: APPS_SCHEMA_VERSION,
            next_seq: 1,
            last_read_seq: 0,
            dropped_count: 0,
            session_dropped_count: BTreeMap::new(),
            session_read_seq: BTreeMap::new(),
            events: Vec::new(),
        }
    }
}

fn valid_topic(topic: &str) -> bool {
    let bytes = topic.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_TOPIC_LEN
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'.' || *b == b'-'
        })
}

/// The topic grammar, callable before an event is built.
///
/// `append` applies it too, but a caller that gates on a user permission has
/// to be able to reject a malformed topic BEFORE raising the sheet — see
/// `agent_post_value`.
pub fn validate_topic(topic: &str) -> Result<(), AppError> {
    if valid_topic(topic) {
        return Ok(());
    }
    Err(AppError::InvalidRequest(format!(
        "invalid mailbox topic {topic:?}: must match ^[a-z0-9][a-z0-9_.-]{{0,63}}$"
    )))
}

impl AppMailbox {
    /// Append one event, evicting the oldest once a cap is hit. Returns the
    /// minted sequence number.
    pub fn append(
        &mut self,
        topic: &str,
        body: serde_json::Value,
        created_at_ms: u64,
    ) -> Result<u64, AppError> {
        self.append_for_session(topic, body, created_at_ms, None)
    }

    /// Append an event for one app-owned Agent session, without mixing it into
    /// the conversation Agent's legacy mailbox cursor.
    pub fn append_for_session(
        &mut self,
        topic: &str,
        body: serde_json::Value,
        created_at_ms: u64,
        session_id: Option<&str>,
    ) -> Result<u64, AppError> {
        validate_topic(topic)?;
        let encoded = serde_json::to_vec(&body)
            .map_err(|error| AppError::InvalidRequest(format!("mailbox body: {error}")))?;
        if encoded.len() > MAX_MAILBOX_BODY_BYTES {
            return Err(AppError::InvalidRequest(format!(
                "mailbox body is {} bytes (limit {MAX_MAILBOX_BODY_BYTES})",
                encoded.len()
            )));
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.events.push(AppMailboxEvent {
            seq,
            topic: topic.to_string(),
            session_id: session_id.map(str::to_owned),
            body,
            created_at_ms,
        });
        // Both caps, because neither alone bounds the document: the count cap
        // admits MAX_MAILBOX_EVENTS x MAX_MAILBOX_BODY_BYTES = 1 MiB, four
        // times what `save_mailbox` accepts, and `drain` advances a cursor
        // without removing anything — so without a byte cap here the file
        // grows until every later save fails and `agent.post` is dead for the
        // life of the app. The byte cap never evicts the event just appended
        // (one event is bounded by MAX_MAILBOX_BODY_BYTES, far under the
        // document limit), so an append always leaves its own event readable.
        while self.events.len() > MAX_MAILBOX_EVENTS
            || (self.events.len() > 1 && self.encoded_len() > MAX_MAILBOX_BYTES)
        {
            let evicted = self.events.remove(0);
            // Only count what nobody read: an event the agent already
            // drained is not a loss, and reporting it as one would make
            // every healthy app look lossy.
            match evicted.session_id.as_deref() {
                Some(session_id) => {
                    let cursor = self.session_read_seq.get(session_id).copied().unwrap_or(0);
                    if evicted.seq > cursor {
                        let dropped = self
                            .session_dropped_count
                            .entry(session_id.to_string())
                            .or_default();
                        *dropped = dropped.saturating_add(1);
                    }
                }
                None if evicted.seq > self.last_read_seq => {
                    self.dropped_count = self.dropped_count.saturating_add(1);
                }
                None => {}
            }
        }
        Ok(seq)
    }

    /// Size of this mailbox exactly as [`save_mailbox`] will write it
    /// (pretty-printed, trailing newline), so `append`'s eviction budget and
    /// the save-time cap cannot disagree.
    fn encoded_len(&self) -> u64 {
        serde_json::to_vec_pretty(self).map_or(0, |body| body.len() as u64 + 1)
    }

    /// Read without advancing the cursor. `after_seq` replays history the
    /// cursor already passed — draining moves a marker, it does not delete.
    #[must_use]
    pub fn peek(&self, after_seq: Option<u64>, limit: usize) -> Vec<&AppMailboxEvent> {
        let floor = after_seq.unwrap_or(self.last_read_seq);
        self.events
            .iter()
            .filter(|event| event.session_id.is_none() && event.seq > floor)
            .take(limit)
            .collect()
    }

    /// Read session-targeted events without advancing that session's cursor.
    #[must_use]
    pub fn peek_for_session(
        &self,
        session_id: &str,
        after_seq: Option<u64>,
        limit: usize,
    ) -> Vec<&AppMailboxEvent> {
        let floor = after_seq
            .or_else(|| self.session_read_seq.get(session_id).copied())
            .unwrap_or(0);
        self.events
            .iter()
            .filter(|event| event.session_id.as_deref() == Some(session_id) && event.seq > floor)
            .take(limit)
            .collect()
    }

    /// Report how many events were lost since the last report, and reset.
    ///
    /// Report-ONCE, not a running total: a reader that keeps being told "3
    /// were dropped" long after it acknowledged them cannot tell a fresh
    /// loss from an old one, so the number stops meaning anything. Only the
    /// consuming path calls this — a peek must not clear a loss the next
    /// real read still has to learn about.
    pub fn take_dropped_count(&mut self) -> u64 {
        std::mem::take(&mut self.dropped_count)
    }

    /// Read and advance the cursor past what was returned.
    pub fn drain(&mut self, limit: usize) -> Vec<AppMailboxEvent> {
        let taken: Vec<AppMailboxEvent> = self
            .events
            .iter()
            .filter(|event| event.session_id.is_none() && event.seq > self.last_read_seq)
            .take(limit)
            .cloned()
            .collect();
        if let Some(last) = taken.last() {
            self.last_read_seq = last.seq;
        }
        taken
    }

    /// Read and advance one app Agent session's independent cursor.
    pub fn drain_for_session(&mut self, session_id: &str, limit: usize) -> Vec<AppMailboxEvent> {
        let floor = self.session_read_seq.get(session_id).copied().unwrap_or(0);
        let taken: Vec<AppMailboxEvent> = self
            .events
            .iter()
            .filter(|event| event.session_id.as_deref() == Some(session_id) && event.seq > floor)
            .take(limit)
            .cloned()
            .collect();
        if let Some(last) = taken.last() {
            self.session_read_seq
                .insert(session_id.to_string(), last.seq);
        }
        taken
    }

    /// Clear and return the loss count for one app Agent session.
    pub fn take_session_dropped_count(&mut self, session_id: &str) -> u64 {
        self.session_dropped_count.remove(session_id).unwrap_or(0)
    }

    fn validate(&self) -> Result<(), AppError> {
        if self.schema_version != APPS_SCHEMA_VERSION {
            return Err(AppError::StorageCorrupt(format!(
                "mailbox schemaVersion {} is unsupported (expected {APPS_SCHEMA_VERSION})",
                self.schema_version
            )));
        }
        Ok(())
    }
}

/// Load an app's mailbox.
///
/// Unreadable bytes are moved aside and an empty mailbox returned: a
/// mailbox is a buffer between two subsystems, so corruption may cost the
/// app its pending events but must never cost it the ability to run — the
/// opposite trade from `permissions.json`, where failing closed is the whole
/// point.
pub fn load_mailbox(layout: &AppLayout) -> Result<AppMailbox, AppError> {
    let relative = layout.mailbox_rel();
    let body = match rooted_fs::read_to_string_limited(layout.root(), &relative, MAX_MAILBOX_BYTES)
    {
        Ok(body) => body,
        Err(FsError::NotFound(_)) => return Ok(AppMailbox::default()),
        Err(error) => return Err(AppError::from_fs("read app mailbox", &error)),
    };
    match serde_json::from_str::<AppMailbox>(&body).map_err(|error| error.to_string()) {
        Ok(mailbox) if mailbox.validate().is_ok() => Ok(mailbox),
        Ok(_) | Err(_) => {
            set_corrupt_mailbox_aside(layout, &relative);
            Ok(AppMailbox::default())
        }
    }
}

fn set_corrupt_mailbox_aside(layout: &AppLayout, relative: &std::path::Path) {
    let from = layout.root().join(relative);
    let to = from.with_extension("corrupt.json");
    if let Err(error) = std::fs::rename(&from, &to) {
        tracing::warn!(
            path = %from.display(),
            %error,
            "could not set a corrupt app mailbox aside; starting empty anyway"
        );
    }
}

/// Atomically persist an app's mailbox.
pub fn save_mailbox(layout: &AppLayout, mailbox: &AppMailbox) -> Result<(), AppError> {
    mailbox.validate()?;
    layout.initialize()?;
    let mut body = serde_json::to_vec_pretty(mailbox)
        .map_err(|error| AppError::Io(format!("serialize app mailbox: {error}")))?;
    body.push(b'\n');
    if body.len() as u64 > MAX_MAILBOX_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "app mailbox is {} bytes (limit {MAX_MAILBOX_BYTES})",
            body.len()
        )));
    }
    rooted_fs::atomic_write(
        layout.root(),
        &layout.mailbox_rel(),
        &body,
        AtomicWriteOptions::default(),
    )
    .map_err(|error| AppError::from_fs("write app mailbox", &error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::AppLayout;

    fn body(text: &str) -> serde_json::Value {
        serde_json::json!({ "text": text })
    }

    #[test]
    fn appended_events_get_monotonic_sequence_numbers() {
        let mut mailbox = AppMailbox::default();
        let first = mailbox
            .append("timer.done", body("一"), 1_000)
            .expect("append");
        let second = mailbox
            .append("timer.done", body("二"), 1_001)
            .expect("append");
        assert!(second > first, "sequence numbers must be monotonic");
        assert_eq!(mailbox.events.len(), 2);
    }

    #[test]
    fn a_bad_topic_or_oversized_body_is_refused() {
        let mut mailbox = AppMailbox::default();
        assert!(mailbox.append("Timer.Done", body("大写"), 1).is_err());
        assert!(mailbox.append("../escape", body("路径"), 1).is_err());
        assert!(mailbox.append("", body("空"), 1).is_err());
        let huge = serde_json::json!({ "text": "字".repeat(20_000) });
        assert!(mailbox.append("timer.done", huge, 1).is_err());
        assert!(mailbox.events.is_empty(), "a refused post must not land");
    }

    #[test]
    fn the_oldest_events_are_dropped_and_counted_once_the_caps_are_hit() {
        let mut mailbox = AppMailbox::default();
        for i in 0..(MAX_MAILBOX_EVENTS + 5) {
            mailbox
                .append("timer.done", body(&format!("{i}")), 1_000 + i as u64)
                .expect("append");
        }
        assert_eq!(mailbox.events.len(), MAX_MAILBOX_EVENTS);
        assert_eq!(
            mailbox.dropped_count, 5,
            "an app must be able to tell that it lost events, not silently miss them"
        );
        assert_eq!(mailbox.events[0].body["text"], "5");
    }

    /// The count cap alone admits MAX_MAILBOX_EVENTS x MAX_MAILBOX_BODY_BYTES
    /// = 1 MiB, four times what `save_mailbox` accepts — and since `drain`
    /// never removes an event, a document that grows past the save cap can
    /// never shrink again, so `agent.post` would fail forever. Every legal
    /// append must therefore leave a document the writer will still take.
    #[test]
    fn a_run_of_max_size_bodies_stays_writable() {
        let mut mailbox = AppMailbox::default();
        let big = serde_json::json!({ "text": "x".repeat(MAX_MAILBOX_BODY_BYTES - 64) });
        for i in 0..(MAX_MAILBOX_EVENTS + 5) {
            let seq = mailbox
                .append("sensor.sample", big.clone(), 1_000 + i as u64)
                .expect("a body under the per-event cap must always be accepted");
            assert!(
                mailbox.events.iter().any(|event| event.seq == seq),
                "an append must never evict the event it just minted"
            );
            let encoded = serde_json::to_vec_pretty(&mailbox)
                .expect("serialize")
                .len()
                + 1;
            assert!(
                encoded as u64 <= MAX_MAILBOX_BYTES,
                "append #{i} left {encoded} bytes, which save_mailbox refuses \
                 (limit {MAX_MAILBOX_BYTES}) — the mailbox is wedged from here on"
            );
        }
        assert!(
            mailbox.dropped_count > 0,
            "the byte cap must report what it evicted, not drop it silently"
        );
    }

    #[test]
    fn peek_leaves_the_cursor_where_drain_advances_it() {
        let mut mailbox = AppMailbox::default();
        for i in 0..3 {
            mailbox
                .append("note.added", body(&format!("{i}")), 1_000 + i)
                .expect("append");
        }

        let peeked = mailbox.peek(None, 10);
        assert_eq!(peeked.len(), 3);
        assert_eq!(mailbox.last_read_seq, 0, "peek must not consume");

        let drained = mailbox.drain(2);
        assert_eq!(drained.len(), 2);
        assert_eq!(mailbox.last_read_seq, drained[1].seq);
        let remaining = mailbox.drain(10);
        assert_eq!(remaining.len(), 1, "a drain resumes after the cursor");
        assert!(mailbox.drain(10).is_empty(), "nothing is left to drain");
    }

    #[test]
    fn session_events_have_an_independent_cursor_and_do_not_leak_to_conversation_reads() {
        let mut mailbox = AppMailbox::default();
        mailbox
            .append("conversation.note", body("conversation"), 1_000)
            .expect("conversation append");
        mailbox
            .append_for_session("agent.note", body("agent"), 1_001, Some("agent-1"))
            .expect("session append");

        assert_eq!(mailbox.peek(None, 10).len(), 1);
        assert_eq!(mailbox.peek_for_session("agent-1", None, 10).len(), 1);
        assert_eq!(mailbox.drain(10)[0].body["text"], "conversation");
        assert_eq!(mailbox.last_read_seq, 1);
        assert_eq!(mailbox.session_read_seq.get("agent-1"), None);

        let drained = mailbox.drain_for_session("agent-1", 10);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].body["text"], "agent");
        assert_eq!(mailbox.session_read_seq.get("agent-1"), Some(&2));
        assert!(mailbox.drain_for_session("agent-1", 10).is_empty());
    }

    #[test]
    fn a_drop_is_reported_once_and_then_cleared() {
        let mut mailbox = AppMailbox::default();
        for i in 0..(MAX_MAILBOX_EVENTS + 2) {
            mailbox
                .append("tick", body(&format!("{i}")), 1_000 + i as u64)
                .expect("append");
        }
        assert_eq!(mailbox.dropped_count, 2);
        assert_eq!(mailbox.take_dropped_count(), 2);
        assert_eq!(
            mailbox.take_dropped_count(),
            0,
            "a loss already reported must not be re-reported forever — a reader could \
             never tell a fresh drop from an acknowledged one"
        );
    }

    #[test]
    fn after_seq_replays_history_the_cursor_already_passed() {
        let mut mailbox = AppMailbox::default();
        for i in 0..3 {
            mailbox
                .append("note.added", body(&format!("{i}")), 1_000 + i)
                .expect("append");
        }
        let drained = mailbox.drain(10);
        assert_eq!(drained.len(), 3);

        let replayed = mailbox.peek(Some(0), 10);
        assert_eq!(
            replayed.len(),
            3,
            "draining moves a cursor; it must not delete history the agent may re-read"
        );
        let tail = mailbox.peek(Some(drained[0].seq), 10);
        assert_eq!(tail.len(), 2);
    }

    #[test]
    fn a_mailbox_round_trips_through_disk() {
        let root = tempfile::tempdir().unwrap();
        let layout = AppLayout::new(root.path(), "abcd1234").unwrap();
        assert_eq!(load_mailbox(&layout).unwrap(), AppMailbox::default());

        let mut mailbox = AppMailbox::default();
        mailbox
            .append("timer.done", body("番茄钟结束"), 1_000)
            .unwrap();
        save_mailbox(&layout, &mailbox).unwrap();
        assert_eq!(load_mailbox(&layout).unwrap(), mailbox);
    }

    /// A mailbox is a buffer, not a ledger: corrupt bytes must cost the app
    /// its pending events, never its ability to run.
    #[test]
    fn a_corrupt_mailbox_is_set_aside_and_rebuilt_empty() {
        let root = tempfile::tempdir().unwrap();
        let layout = AppLayout::new(root.path(), "abcd1234").unwrap();
        layout.initialize().unwrap();
        let path = root.path().join(layout.mailbox_rel());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ this is not json").unwrap();

        let recovered = load_mailbox(&layout).expect("a corrupt mailbox must not fail the app");
        assert_eq!(recovered, AppMailbox::default());
        assert!(
            std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().contains("corrupt")),
            "the unreadable bytes must be kept aside for diagnosis, not deleted"
        );
    }
}
