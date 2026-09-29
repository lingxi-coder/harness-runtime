//! Property tests for `reduce`. These run with proptest at 256 cases by
//! default; the CI `--features 10k-iterations` profile bumps to 10K.

use lingxi_core::types::{MessageId, RequestId, SessionId};
use lingxi_core::{reduce, ConversationState, Event, SessionState};
use proptest::prelude::*;

fn arb_event() -> impl Strategy<Value = Event> {
    prop_oneof![
        ("[a-z ]{0,100}").prop_map(|s| Event::UserMessage {
            message_id: MessageId::new(),
            request_id: RequestId::new(),
            content: s,
        }),
        Just(Event::UserInterrupt),
        Just(Event::UserExit),
    ]
}

proptest! {
    /// The reducer is total — it never panics on any (state, event) pair.
    #[test]
    fn reducer_is_total(events in proptest::collection::vec(arb_event(), 0..50)) {
        let mut state = ConversationState::Idle {
            session: SessionState::empty(SessionId::nil(), "x".into()),
        };
        for e in events {
            let (next, _effects) = reduce(state, e);
            state = next;
        }
    }

    /// Terminated is absorbing.
    #[test]
    fn terminated_absorbs(events in proptest::collection::vec(arb_event(), 0..20)) {
        let session = SessionState::empty(SessionId::nil(), "x".into());
        let mut state = ConversationState::Terminated { session, reason: "x".into() };
        for e in events {
            let (next, effects) = reduce(state, e);
            prop_assert!(next.is_terminal());
            prop_assert!(effects.is_empty());
            state = next;
        }
    }

    /// Token usage is monotonically non-decreasing.
    #[test]
    fn token_usage_monotonic(events in proptest::collection::vec(arb_event(), 0..50)) {
        let mut state = ConversationState::Idle {
            session: SessionState::empty(SessionId::nil(), "x".into()),
        };
        let mut prev = 0u64;
        for e in events {
            let (next, _) = reduce(state, e);
            let cur = next.session().usage.0.total_tokens();
            prop_assert!(cur >= prev, "usage decreased: {prev} -> {cur}");
            prev = cur;
            state = next;
        }
    }
}
