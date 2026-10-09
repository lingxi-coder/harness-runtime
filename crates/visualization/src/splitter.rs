//! Split assistant text into narration and visualization references.
//!
//! The same [`ReferenceSplitter`] serves the live stream (fed one delta at a
//! time) and transcript replay ([`project_text`], fed the whole block), so the
//! two cannot disagree about where a reference starts or what it resolves to.
//!
//! Text is released as soon as its line can no longer be a reference; a line
//! that might still be one is held back. Once the full prefix has arrived the
//! splitter announces [`SplitEvent::Pending`] so a client can reserve space,
//! and the end of the line settles it as [`SplitEvent::Ready`],
//! [`SplitEvent::Unavailable`] or [`SplitEvent::Discarded`].

use crate::reference::{DirectiveLine, LineClassifier, VisualizationRef};

/// One output of the splitter, in stream order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitEvent {
    /// Narration text, verbatim.
    Text(String),
    /// A reference line has started; its outcome follows.
    Pending,
    /// The line resolved to this reference.
    Ready(VisualizationRef),
    /// The line looked like a reference but did not parse.
    Unavailable,
    /// The line started like a reference but never completed; drop the placeholder.
    Discarded,
}

/// A replayed or completed block segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Narration text.
    Text(String),
    /// A visualization slot; `None` renders as "unavailable".
    Visualization(Option<VisualizationRef>),
}

/// Incremental splitter for ONE assistant text block.
#[derive(Debug, Default)]
pub struct ReferenceSplitter {
    classifier: LineClassifier,
    /// Bytes of the current line since the last `\n`.
    line: String,
    /// The current line is still a possible reference and nothing of it has
    /// been released.
    released: bool,
    pending_announced: bool,
}

impl ReferenceSplitter {
    /// A splitter positioned at the start of a block.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// True when a partial line is being held back.
    #[must_use]
    pub fn is_holding(&self) -> bool {
        !self.released && (!self.line.is_empty() || self.pending_announced)
    }

    /// Feed the next delta.
    pub fn push(&mut self, chunk: &str) -> Vec<SplitEvent> {
        let mut events = Vec::new();
        let mut text = String::new();
        for piece in chunk.split_inclusive('\n') {
            let (body, newline) = match piece.strip_suffix('\n') {
                Some(body) => (body, true),
                None => (piece, false),
            };
            self.line.push_str(body);
            if self.released {
                text.push_str(body);
            } else if !self.classifier.could_be_reference(&self.line) {
                self.released = true;
                text.push_str(&self.line);
            } else if !self.pending_announced && self.classifier.has_full_prefix(&self.line) {
                flush(&mut text, &mut events);
                events.push(SplitEvent::Pending);
                self.pending_announced = true;
            }
            if newline {
                self.end_line(&mut text, &mut events, true);
            }
        }
        flush(&mut text, &mut events);
        events
    }

    /// End of the block: settle a held line as if it ended here, then reset
    /// for the next block.
    pub fn finish(&mut self) -> Vec<SplitEvent> {
        let mut events = Vec::new();
        let mut text = String::new();
        if !self.line.is_empty() || self.pending_announced {
            self.end_line(&mut text, &mut events, false);
        }
        flush(&mut text, &mut events);
        *self = Self::default();
        events
    }

    /// The stream was cut (cancel, refusal, fallback retraction): drop any
    /// held partial line and reset. Returns `Discarded` when a placeholder was
    /// announced so the client can remove it.
    pub fn abort(&mut self) -> Vec<SplitEvent> {
        let announced = self.pending_announced && !self.released;
        *self = Self::default();
        if announced {
            vec![SplitEvent::Discarded]
        } else {
            Vec::new()
        }
    }

    fn end_line(&mut self, text: &mut String, events: &mut Vec<SplitEvent>, newline: bool) {
        let line = std::mem::take(&mut self.line);
        let decision = self.classifier.classify(&line);
        if self.released {
            if newline {
                text.push('\n');
            }
        } else {
            match decision {
                Some(DirectiveLine::Ready(reference)) => {
                    flush(text, events);
                    events.push(SplitEvent::Ready(reference));
                }
                Some(DirectiveLine::Unavailable) => {
                    flush(text, events);
                    events.push(SplitEvent::Unavailable);
                }
                Some(DirectiveLine::Discarded) => {
                    flush(text, events);
                    if self.pending_announced {
                        events.push(SplitEvent::Discarded);
                    }
                }
                None => {
                    text.push_str(&line);
                    if newline {
                        text.push('\n');
                    }
                }
            }
        }
        self.released = false;
        self.pending_announced = false;
    }
}

fn flush(text: &mut String, events: &mut Vec<SplitEvent>) {
    if !text.is_empty() {
        events.push(SplitEvent::Text(std::mem::take(text)));
    }
}

/// Settled segments of one complete text block, adjacent text merged.
#[must_use]
pub fn project_text(text: &str) -> Vec<Segment> {
    if !text.contains(crate::reference::REFERENCE_PREFIX) {
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![Segment::Text(text.to_string())]
        };
    }
    let mut splitter = ReferenceSplitter::new();
    let mut events = splitter.push(text);
    events.extend(splitter.finish());
    settle(events)
}

/// Collapse a live event sequence to the segments a completed message holds.
#[must_use]
pub fn settle(events: impl IntoIterator<Item = SplitEvent>) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    for event in events {
        match event {
            SplitEvent::Text(text) => {
                if let Some(Segment::Text(previous)) = segments.last_mut() {
                    previous.push_str(&text);
                } else {
                    segments.push(Segment::Text(text));
                }
            }
            SplitEvent::Ready(reference) => segments.push(Segment::Visualization(Some(reference))),
            SplitEvent::Unavailable => segments.push(Segment::Visualization(None)),
            SplitEvent::Pending | SplitEvent::Discarded => {}
        }
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reference::VisualizationId;
    use proptest::prelude::*;

    const LINE: &str = "::lingxi-visualization{id=\"chart\" rev=\"2\"}";

    fn chart() -> VisualizationRef {
        VisualizationRef {
            id: VisualizationId::parse("chart").unwrap(),
            revision: 2,
        }
    }

    fn live(chunks: &[&str]) -> Vec<SplitEvent> {
        let mut splitter = ReferenceSplitter::new();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(splitter.push(chunk));
        }
        events.extend(splitter.finish());
        events
    }

    #[test]
    fn text_without_references_is_released_immediately() {
        let mut splitter = ReferenceSplitter::new();
        assert_eq!(
            splitter.push("Hello"),
            vec![SplitEvent::Text("Hello".into())]
        );
        assert!(!splitter.is_holding());
        assert_eq!(
            splitter.push(" world\n"),
            vec![SplitEvent::Text(" world\n".into())]
        );
    }

    #[test]
    fn reference_line_streams_pending_then_ready() {
        let events = live(&[
            "Before\n::lingxi-",
            "visualization{id=\"chart\"",
            " rev=\"2\"}\nAfter",
        ]);
        assert_eq!(
            events,
            vec![
                SplitEvent::Text("Before\n".into()),
                SplitEvent::Pending,
                SplitEvent::Ready(chart()),
                SplitEvent::Text("After".into()),
            ]
        );
    }

    #[test]
    fn incomplete_reference_is_hidden_while_streaming() {
        let mut splitter = ReferenceSplitter::new();
        let events = splitter.push("Before\n::lingxi-visualization{id=\"chart");
        assert_eq!(
            events,
            vec![SplitEvent::Text("Before\n".into()), SplitEvent::Pending]
        );
        assert!(splitter.is_holding());
        assert_eq!(splitter.finish(), vec![SplitEvent::Discarded]);
    }

    #[test]
    fn abort_drops_the_held_line() {
        let mut splitter = ReferenceSplitter::new();
        splitter.push("::lingxi-visualization{id=");
        assert_eq!(splitter.abort(), vec![SplitEvent::Discarded]);
        assert!(!splitter.is_holding());
        let mut quiet = ReferenceSplitter::new();
        quiet.push("  ");
        assert_eq!(quiet.abort(), Vec::new());
    }

    #[test]
    fn unparsable_closed_reference_is_unavailable() {
        assert_eq!(
            project_text("::lingxi-visualization{id=\"chart\" rev=\"two\"}"),
            vec![Segment::Visualization(None)]
        );
    }

    #[test]
    fn code_blocks_and_inline_mentions_keep_their_text() {
        let text = format!("```\n{LINE}\n```\nsay {LINE}\n    {LINE}\n");
        assert_eq!(project_text(&text), vec![Segment::Text(text.clone())]);
    }

    #[test]
    fn replay_matches_upstream_ordering() {
        let text = format!("Intro\n\n{LINE}\n\nOutro");
        assert_eq!(
            project_text(&text),
            vec![
                Segment::Text("Intro\n\n".into()),
                Segment::Visualization(Some(chart())),
                Segment::Text("\nOutro".into()),
            ]
        );
    }

    #[test]
    fn a_reference_only_message_keeps_its_slot() {
        assert_eq!(
            project_text(LINE),
            vec![Segment::Visualization(Some(chart()))]
        );
        assert_eq!(project_text(""), Vec::new());
    }

    #[test]
    fn every_chunking_settles_to_the_replay_projection() {
        let samples = [
            format!("a\n{LINE}\nb\n"),
            format!("```\n{LINE}\n```\n{LINE}"),
            format!("   {LINE}\r\n  ::lingxi-visualization{{id=\"x\"\nnext"),
            format!(":) {LINE}\n::\n:::\n{LINE}  \n"),
            "plain\ntext\n   \n".to_string(),
            format!("{LINE}\n{LINE}"),
        ];
        for sample in samples {
            let expected = project_text(&sample);
            for split in 0..=sample.len() {
                if !sample.is_char_boundary(split) {
                    continue;
                }
                let (left, right) = sample.split_at(split);
                assert_eq!(
                    settle(live(&[left, right])),
                    expected,
                    "split {split} of {sample:?}"
                );
            }
            let singles: Vec<String> = sample.chars().map(String::from).collect();
            let refs: Vec<&str> = singles.iter().map(String::as_str).collect();
            assert_eq!(settle(live(&refs)), expected, "char-by-char {sample:?}");
        }
    }

    fn fragment() -> impl Strategy<Value = String> {
        prop_oneof![
            Just(LINE.to_string()),
            Just("::lingxi-visualization{".to_string()),
            Just("id=\"a\" rev=\"1\"}".to_string()),
            Just("```".to_string()),
            Just("~~~".to_string()),
            Just("\n".to_string()),
            Just("\r\n".to_string()),
            Just("   ".to_string()),
            Just("\t".to_string()),
            Just(":".to_string()),
            "[a-z }{\"=:`~]{0,6}",
            "\\PC{0,3}",
        ]
    }

    proptest! {
        #[test]
        fn arbitrary_chunking_is_invariant(parts in proptest::collection::vec(fragment(), 0..24), cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..6)) {
            let text: String = parts.concat();
            let expected = project_text(&text);
            let mut offsets: Vec<usize> = cuts.iter().map(|cut| cut.index(text.len() + 1)).filter(|&at| text.is_char_boundary(at)).collect();
            offsets.sort_unstable();
            offsets.dedup();
            let mut chunks = Vec::new();
            let mut start = 0;
            for at in offsets {
                chunks.push(&text[start..at]);
                start = at;
            }
            chunks.push(&text[start..]);
            let settled = settle(live(&chunks));
            prop_assert_eq!(&settled, &expected);
            // Narration bytes are never lost or invented outside reference lines.
            let rebuilt: String = settled.iter().map(|segment| match segment {
                Segment::Text(text) => text.clone(),
                Segment::Visualization(_) => String::new(),
            }).collect();
            prop_assert!(rebuilt.len() <= text.len());
        }
    }
}
