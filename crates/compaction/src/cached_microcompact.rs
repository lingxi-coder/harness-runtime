//! Cached microcompact — same-input result cache for the microcompact layer.

use crate::microcompact::MicrocompactResult;
use lingxi_core::types::ConversationMessage;
use std::collections::{hash_map::DefaultHasher, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::SystemTime;

/// Result of a cached microcompact lookup.
#[derive(Debug, Clone)]
pub struct CachedMicrocompactResult {
    /// The microcompact result, either freshly computed or cloned from cache.
    pub result: MicrocompactResult,
    /// Whether `result` came from the same-input cache.
    pub cache_hit: bool,
}

/// Same-input cache for microcompact results.
///
/// The key is a stable hash of the input message list. An empty cache has no
/// behavioral effect: the first call computes through the supplied function,
/// stores that result, and returns it unchanged.
#[derive(Default)]
pub struct CachedMicrocompact {
    entries: Mutex<HashMap<u64, MicrocompactResult>>,
}

impl CachedMicrocompact {
    /// Return a cached result for `messages`, or compute and store one on miss.
    pub fn compact_with(
        &self,
        messages: Vec<ConversationMessage>,
        now: SystemTime,
        compute: impl FnOnce(Vec<ConversationMessage>, SystemTime) -> MicrocompactResult,
    ) -> CachedMicrocompactResult {
        self.compact_with_key(messages, (), now, compute)
    }

    /// Same as [`Self::compact_with`], with caller-supplied key material such as
    /// microcompact config. This avoids reusing a summary computed under a
    /// different `keep_recent` / threshold policy.
    pub fn compact_with_key(
        &self,
        messages: Vec<ConversationMessage>,
        key_material: impl Hash,
        now: SystemTime,
        compute: impl FnOnce(Vec<ConversationMessage>, SystemTime) -> MicrocompactResult,
    ) -> CachedMicrocompactResult {
        let key = input_hash(&messages, key_material);
        if let Some(result) = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned()
        {
            return CachedMicrocompactResult {
                result,
                cache_hit: true,
            };
        }

        let result = compute(messages, now);
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, result.clone());

        CachedMicrocompactResult {
            result,
            cache_hit: false,
        }
    }
}

fn input_hash(messages: &[ConversationMessage], key_material: impl Hash) -> u64 {
    let bytes = serde_json::to_vec(messages).expect("ConversationMessage serializes");
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    key_material.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::microcompact::MicrocompactResult;
    use lingxi_core::types::{ConversationMessage, MessageId};
    use std::cell::Cell;
    use std::time::SystemTime;

    fn messages(body: &str) -> Vec<ConversationMessage> {
        vec![ConversationMessage::user(
            MessageId::new(),
            body.to_string(),
        )]
    }

    fn computed_summary(input: Vec<ConversationMessage>) -> MicrocompactResult {
        MicrocompactResult {
            messages: vec![ConversationMessage::user(
                MessageId::new(),
                format!("summary:{}", input[0].text_content()),
            )],
            cleared_count: 1,
            tokens_saved: 42,
        }
    }

    #[test]
    fn miss_computes_and_stores_summary() {
        let cache = CachedMicrocompact::default();
        let calls = Cell::new(0);

        let out = cache.compact_with(messages("alpha"), SystemTime::UNIX_EPOCH, |input, _now| {
            calls.set(calls.get() + 1);
            computed_summary(input)
        });

        assert!(!out.cache_hit, "empty cache must miss");
        assert_eq!(calls.get(), 1, "miss computes exactly once");
        assert_eq!(out.result.tokens_saved, 42);
        assert_eq!(out.result.messages[0].text_content(), "summary:alpha");
    }

    #[test]
    fn same_input_hits_cached_summary_without_recomputing() {
        let cache = CachedMicrocompact::default();
        let calls = Cell::new(0);
        let input = messages("alpha");

        let first = cache.compact_with(input.clone(), SystemTime::UNIX_EPOCH, |input, _now| {
            calls.set(calls.get() + 1);
            computed_summary(input)
        });
        let second = cache.compact_with(input, SystemTime::UNIX_EPOCH, |_input, _now| {
            calls.set(calls.get() + 1);
            MicrocompactResult {
                messages: messages("wrong-recomputed-summary"),
                cleared_count: 999,
                tokens_saved: 999,
            }
        });

        assert!(!first.cache_hit);
        assert!(second.cache_hit, "same input must hit");
        assert_eq!(calls.get(), 1, "hit must not recompute");
        assert_eq!(second.result.tokens_saved, first.result.tokens_saved);
        assert_eq!(
            second.result.messages[0].text_content(),
            first.result.messages[0].text_content()
        );
    }

    #[test]
    fn different_key_material_misses_even_for_same_messages() {
        let cache = CachedMicrocompact::default();
        let input = messages("alpha");
        let first = cache.compact_with_key(input.clone(), 1_u8, SystemTime::UNIX_EPOCH, |i, _| {
            computed_summary(i)
        });
        let second = cache.compact_with_key(input, 2_u8, SystemTime::UNIX_EPOCH, |i, _| {
            computed_summary(i)
        });
        assert!(!first.cache_hit);
        assert!(!second.cache_hit, "different config/key material must miss");
    }
}
