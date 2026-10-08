//! Per-context native nested-memory triggers, independent of read-state LRU.

use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Default)]
struct State {
    triggers: Vec<PathBuf>,
    pending: Vec<PathBuf>,
    consumed: Vec<PathBuf>,
}

/// One tool-use context's insertion-ordered trigger arrays (native h7t).
/// Cloned tool invocations share their owner's Arc; child contexts own fresh
/// arrays. Cache insertion, eviction, restore and seeding never enqueue paths.
#[derive(Debug, Default)]
pub struct NestedMemoryTriggers {
    state: Mutex<State>,
}

impl NestedMemoryTriggers {
    /// Native input retraction restores the saved array with push(...paths).
    pub fn restore(&self, paths: impl IntoIterator<Item = PathBuf>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .triggers
            .extend(paths);
    }

    /// A real Read/readFor producer enqueues a path once until consumption.
    pub fn enqueue(&self, path: PathBuf) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.triggers.contains(&path) {
            state.triggers.push(path);
        }
    }

    /// Pending propagation/retraction paths, before the main-context merge.
    pub fn append_pending(&self, paths: impl IntoIterator<Item = PathBuf>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .extend(paths);
    }

    /// Native h7t merges pending paths only for the main context, records every
    /// pending path, clears pending, then checks the disable flag.
    pub fn begin(&self, is_agent: bool, disabled: bool) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !is_agent {
            let pending = std::mem::take(&mut state.pending);
            for path in &pending {
                if !state.triggers.contains(path) {
                    state.triggers.push(path.clone());
                }
            }
            state.consumed.extend(pending);
        }
        if disabled {
            state.triggers.clear();
        }
        !state.triggers.is_empty()
    }

    /// Iterate the live array across awaits. A failure/cancellation before the
    /// end leaves triggers in place, as native does. Finish clears atomically;
    /// a producer arriving after completion belongs to the next consumption.
    pub fn next(&self, cursor: &mut usize) -> Option<PathBuf> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(path) = state.triggers.get(*cursor).cloned() {
            *cursor += 1;
            Some(path)
        } else {
            state.triggers.clear();
            None
        }
    }

    /// Input-processing ownership resets the consumed ledger between turns.
    pub fn take_consumed(&self) -> Vec<PathBuf> {
        std::mem::take(
            &mut self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .consumed,
        )
    }

    /// Current queued paths, without consuming or changing order.
    #[must_use]
    pub fn queued(&self) -> Vec<PathBuf> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .triggers
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn native_287_consumer_goldens() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/nested_trigger_2_1_287.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let input = &case["input"];
            let paths = |key: &str| {
                input[key]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|path| PathBuf::from(path.as_str().unwrap()))
                    .collect::<Vec<_>>()
            };
            let queue = NestedMemoryTriggers::default();
            queue.restore(paths("triggers"));
            queue.append_pending(paths("pending"));
            let mut calls = Vec::new();
            let mut attachments = Vec::new();
            let mut cursor = 0;
            let mut failed = false;
            let mut appended = false;
            if queue.begin(
                input["agent"].as_bool().unwrap(),
                input["disabled"].as_bool().unwrap(),
            ) {
                while let Some(path) = queue.next(&mut cursor) {
                    calls.push(path.clone());
                    if !appended {
                        if let Some(late) = input["appendDuring"].as_str() {
                            queue.enqueue(late.into());
                            appended = true;
                        }
                    }
                    if input["throwOn"]
                        .as_str()
                        .is_some_and(|failure| path == std::path::Path::new(failure))
                    {
                        failed = true;
                        break;
                    }
                    attachments.push(json!({"path":path}));
                }
            }
            let state = queue.state.lock().unwrap();
            assert_eq!(
                json!({"queued":state.triggers,"pending":state.pending,"consumed":state.consumed,"calls":calls,
                "attachments":if failed {Value::Null} else {json!(attachments)},"failed":failed}),
                case["expected"],
                "{input}"
            );
        }
    }

    #[test]
    fn producers_dedup_until_consumption_then_allow_another_read() {
        let queue = NestedMemoryTriggers::default();
        queue.enqueue("file".into());
        queue.enqueue("file".into());
        let mut cursor = 0;
        assert!(queue.begin(false, false));
        assert_eq!(queue.next(&mut cursor), Some("file".into()));
        assert_eq!(queue.next(&mut cursor), None);
        queue.enqueue("file".into());
        assert_eq!(queue.queued(), vec![PathBuf::from("file")]);
    }
}
