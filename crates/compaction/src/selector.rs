//! The `/rewind` message-selector summarize split — oracle `zir`
//! (`src_169588164.js`), the function behind "Summarize from here" and
//! "Summarize up to here".
//!
//! This module owns the PURE part: given the conversation and the index of the
//! chosen message, which messages are summarized and which survive verbatim.
//! The model call and the history commit live in the orchestrator.
//!
//! ```text
//! up_to:  summarize [0, n)   keep [n, ..)   summary goes FIRST
//! from:   summarize [n, ..)  keep [0, n)    summary goes LAST
//! ```
//!
//! 🚨 The mapping reads backwards from the labels, so it is worth stating
//! plainly: "Summarize **up to** here" summarizes the PAST and you keep
//! standing where you are; "Summarize **from** here" summarizes the messages
//! from the chosen point onwards and rewinds your context to just before it.
//! The two prompt bodies encode the same thing —
//! [`crate::prompt::SUMMARIZE_UP_TO_PROMPT`] says the summary "will be placed
//! at the start of a continuing session", while
//! [`crate::prompt::SUMMARIZE_FROM_PROMPT`] says the earlier messages "are
//! being kept intact".

use crate::prompt::SummarizeDirection;
use lingxi_core::types::ConversationMessage;

/// `"Nothing to summarize before the selected message."` — oracle `zir`'s
/// `up_to` guard, byte-exact.
pub const NOTHING_BEFORE: &str = "Nothing to summarize before the selected message.";

/// `"Nothing to summarize after the selected message."` — the `from` guard.
pub const NOTHING_AFTER: &str = "Nothing to summarize after the selected message.";

/// One direction's split of the conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct SummarizeSplit {
    /// The messages handed to the summarizer (`V`).
    pub to_summarize: Vec<ConversationMessage>,
    /// The messages that survive verbatim (`re` / `messagesToKeep`), already
    /// filtered for the direction.
    pub to_keep: Vec<ConversationMessage>,
    /// Which half is which.
    pub direction: SummarizeDirection,
}

/// A prior compaction artifact — oracle `uye`: a `compact_boundary` system
/// message, or a user message flagged `isCompactSummary`.
///
/// Only the `up_to` keep-side strips these. That asymmetry is deliberate
/// upstream and worth not "tidying": `up_to` replaces the conversation's whole
/// past with one fresh summary, so an older boundary and its summary inside the
/// kept tail would describe history that is no longer there. `from` keeps the
/// genuine earlier prefix, compaction artifacts included, because that prefix
/// is still the real conversation.
#[must_use]
pub fn is_prior_compaction_artifact(message: &ConversationMessage) -> bool {
    crate::boundary::is_compact_boundary(message)
        || matches!(
            message,
            ConversationMessage::User {
                is_compact_summary: true,
                ..
            }
        )
}

/// Split `messages` at `index` for `direction` — oracle `zir`'s opening:
///
/// ```js
/// V  = S==="up_to" ? e.slice(0,n) : e.slice(n)
/// re = S==="up_to" ? e.slice(n).filter(r => r.type!=="progress" && !Zi(r)
///                                           && !(r.type==="user" && r.isCompactSummary))
///                  : e.slice(0,n).filter(r => r.type!=="progress")
/// ```
///
/// ⚠️ The `type!=="progress"` conjunct has NO analogue here and is not a
/// missing filter: progress rows are a transcript-render concern upstream and
/// never enter this port's model-context history
/// (`ConversationMessage` is `User` | `Assistant` | `System`). Recorded rather
/// than silently dropped, so nobody re-derives it as a gap.
///
/// # Errors
/// Returns the byte-exact user-facing sentence when the SUMMARIZE side is
/// empty — [`NOTHING_BEFORE`] for `up_to`, [`NOTHING_AFTER`] for `from`.
pub fn split_at(
    messages: &[ConversationMessage],
    index: usize,
    direction: SummarizeDirection,
) -> Result<SummarizeSplit, &'static str> {
    let index = index.min(messages.len());
    let (to_summarize, to_keep): (Vec<_>, Vec<_>) = match direction {
        SummarizeDirection::UpTo => (
            messages[..index].to_vec(),
            messages[index..]
                .iter()
                .filter(|message| !is_prior_compaction_artifact(message))
                .cloned()
                .collect(),
        ),
        SummarizeDirection::From => (messages[index..].to_vec(), messages[..index].to_vec()),
    };

    if to_summarize.is_empty() {
        return Err(match direction {
            SummarizeDirection::UpTo => NOTHING_BEFORE,
            SummarizeDirection::From => NOTHING_AFTER,
        });
    }

    Ok(SummarizeSplit {
        to_summarize,
        to_keep,
        direction,
    })
}

/// Locate `uuid` in `messages`, for the picker's chosen message.
#[must_use]
pub fn index_of(messages: &[ConversationMessage], uuid: &str) -> Option<usize> {
    messages
        .iter()
        .position(|message| message.id().to_string() == uuid)
}

impl SummarizeSplit {
    /// The context the summarizer is given — oracle `Ze = S==="up_to" ? V : e`.
    ///
    /// 🚨 `from` sends the WHOLE conversation, not just the half being
    /// summarized. It has to: `SUMMARIZE_FROM_PROMPT` asks for a summary of
    /// "the RECENT portion … the messages that follow earlier retained
    /// context", and a summarizer shown only the tail cannot tell what that
    /// earlier context was. `up_to` sends only the summarized half because
    /// there is nothing earlier.
    #[must_use]
    pub fn summarizer_context(&self, full: &[ConversationMessage]) -> Vec<ConversationMessage> {
        match self.direction {
            SummarizeDirection::UpTo => self.to_summarize.clone(),
            SummarizeDirection::From => full.to_vec(),
        }
    }

    /// The post-summarize history order: `up_to` puts the summary FIRST and the
    /// kept tail after it; `from` keeps the earlier prefix and appends the
    /// summary.
    #[must_use]
    pub fn assemble(&self, summary: Vec<ConversationMessage>) -> Vec<ConversationMessage> {
        match self.direction {
            SummarizeDirection::UpTo => {
                let mut out = summary;
                out.extend(self.to_keep.iter().cloned());
                out
            }
            SummarizeDirection::From => {
                let mut out = self.to_keep.clone();
                out.extend(summary);
                out
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::MessageId;

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.to_string())
    }
    fn assistant(text: &str) -> ConversationMessage {
        ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::Text {
                text: text.to_string(), citations: None,
            }],
            stop_reason: None,
        }
    }
    fn old_summary() -> ConversationMessage {
        ConversationMessage::compact_summary(MessageId::new(), "an older summary".into())
    }

    #[test]
    fn up_to_retains_an_ordinary_system_message_with_compact_body() {
        let ordinary = ConversationMessage::System { api_system: None,
            id: MessageId::new(),
            content: crate::boundary::BOUNDARY_CONTENT.into(),
            subtype: None,
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        };
        let mut typed = crate::boundary::create_compact_boundary(
            crate::boundary::CompactTrigger::Manual,
            0,
            None,
            None,
            None,
            &[],
        )
        .0;
        if let ConversationMessage::System { content, .. } = &mut typed {
            *content = "typed boundary with arbitrary body".into();
        }
        let tail = assistant("tail");
        let messages = vec![
            user("summarize this"),
            ordinary.clone(),
            typed,
            old_summary(),
            tail.clone(),
        ];
        let split = split_at(&messages, 1, SummarizeDirection::UpTo).unwrap();
        assert_eq!(split.to_keep, vec![ordinary, tail]);
        assert_eq!(texts(&split.to_summarize), ["summarize this"]);
    }

    fn texts(messages: &[ConversationMessage]) -> Vec<String> {
        messages
            .iter()
            .map(|message| match message {
                ConversationMessage::User { content, .. }
                | ConversationMessage::Assistant { content, .. } => content
                    .iter()
                    .map(|block| match block {
                        lingxi_core::types::ContentBlock::Text { text, .. } => text.clone(),
                        _ => String::new(),
                    })
                    .collect::<String>(),
                ConversationMessage::System { content, .. } => content.clone(),
            })
            .collect()
    }

    fn convo() -> Vec<ConversationMessage> {
        vec![
            user("one"),
            assistant("two"),
            user("three"),
            assistant("four"),
        ]
    }

    /// 🚨 The property that decides whether this feature does the right thing
    /// at all: which half of the conversation each label summarizes.
    #[test]
    fn each_direction_summarizes_the_half_its_label_names() {
        let messages = convo();

        let up_to = split_at(&messages, 2, SummarizeDirection::UpTo).expect("split");
        assert_eq!(texts(&up_to.to_summarize), ["one", "two"]);
        assert_eq!(texts(&up_to.to_keep), ["three", "four"]);

        let from = split_at(&messages, 2, SummarizeDirection::From).expect("split");
        assert_eq!(texts(&from.to_summarize), ["three", "four"]);
        assert_eq!(texts(&from.to_keep), ["one", "two"]);
    }

    /// …and where the summary then sits relative to the survivors.
    #[test]
    fn the_summary_lands_on_the_side_the_prompt_promises() {
        let messages = convo();
        let summary = vec![ConversationMessage::compact_summary(
            MessageId::new(),
            "SUMMARY".into(),
        )];

        let up_to = split_at(&messages, 2, SummarizeDirection::UpTo).expect("split");
        assert_eq!(
            texts(&up_to.assemble(summary.clone())),
            ["SUMMARY", "three", "four"],
            "up_to: the summary replaces the past, so it goes first"
        );

        let from = split_at(&messages, 2, SummarizeDirection::From).expect("split");
        assert_eq!(
            texts(&from.assemble(summary)),
            ["one", "two", "SUMMARY"],
            "from: the earlier messages survive, so the summary goes last"
        );
    }

    /// `Ze = S==="up_to" ? V : e`.
    #[test]
    fn from_summarizes_against_the_whole_conversation() {
        let messages = convo();
        let from = split_at(&messages, 2, SummarizeDirection::From).expect("split");
        assert_eq!(
            texts(&from.summarizer_context(&messages)),
            ["one", "two", "three", "four"],
            "`from` must show the summarizer the earlier context it is told to \
             assume, or the prompt asks for something impossible"
        );

        let up_to = split_at(&messages, 2, SummarizeDirection::UpTo).expect("split");
        assert_eq!(texts(&up_to.summarizer_context(&messages)), ["one", "two"]);
    }

    /// `uye` — only `up_to` strips prior compaction artifacts from its keep set.
    #[test]
    fn only_up_to_strips_a_previous_summary_from_what_it_keeps() {
        let messages = vec![
            user("one"),
            old_summary(),
            assistant("two"),
            user("three"),
            old_summary(),
            assistant("four"),
        ];

        let up_to = split_at(&messages, 3, SummarizeDirection::UpTo).expect("split");
        assert_eq!(
            texts(&up_to.to_keep),
            ["three", "four"],
            "the kept tail must lose the older summary — the new one supersedes it"
        );
        assert_eq!(
            texts(&up_to.to_summarize),
            ["one", "an older summary", "two"],
            "…but the SUMMARIZE side keeps it: it is part of what is being summarized"
        );

        let from = split_at(&messages, 3, SummarizeDirection::From).expect("split");
        assert_eq!(
            texts(&from.to_keep),
            ["one", "an older summary", "two"],
            "`from` keeps the genuine earlier prefix, compaction artifacts included"
        );
    }

    /// Both byte-exact guards, and which one goes with which direction.
    #[test]
    fn an_empty_summarize_side_reports_the_oracles_sentence() {
        let messages = convo();
        assert_eq!(
            split_at(&messages, 0, SummarizeDirection::UpTo).unwrap_err(),
            "Nothing to summarize before the selected message."
        );
        assert_eq!(
            split_at(&messages, messages.len(), SummarizeDirection::From).unwrap_err(),
            "Nothing to summarize after the selected message."
        );
        // The opposite ends are fine: there is still a half to summarize.
        assert!(split_at(&messages, 0, SummarizeDirection::From).is_ok());
        assert!(split_at(&messages, messages.len(), SummarizeDirection::UpTo).is_ok());
    }

    #[test]
    fn index_of_finds_the_picked_message() {
        let messages = convo();
        let uuid = messages[2].id().to_string();
        assert_eq!(index_of(&messages, &uuid), Some(2));
        assert_eq!(index_of(&messages, "not-a-uuid"), None);
    }
}
