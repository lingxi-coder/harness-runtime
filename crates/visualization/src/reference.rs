//! The reference an assistant writes on its own line to place a published
//! visualization, and the line classifier that live streaming and transcript
//! replay share.
//!
//! ```text
//! ::lingxi-visualization{id="<id>" rev="<n>"}
//! ```
//!
//! Recognition follows the upstream terminal renderer: only a whole line that
//! is exactly one directive counts, a fenced code block keeps its text, an
//! unparsable directive that still ends in `}` degrades to an "unavailable"
//! card, and anything shorter is hidden. Unlike upstream the line may carry at
//! most three leading spaces (four is an indented code block in `CommonMark`)
//! and no tabs, so the streaming splitter can decide a line from its prefix.

use std::fmt;

/// Start of a reference line; owned by `branding`.
pub const REFERENCE_PREFIX: &str = branding::VISUALIZATION_REFERENCE_PREFIX;

/// Longest accepted visualization id.
pub const MAX_ID_LEN: usize = 64;

/// Leading spaces a reference line may carry before it reads as indented code.
const MAX_INDENT: usize = 3;

/// A visualization id: `[A-Za-z0-9_-]{1,64}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VisualizationId(String);

impl VisualizationId {
    /// Validate `value` as an id.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let valid = !value.is_empty()
            && value.len() <= MAX_ID_LEN
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        valid.then(|| Self(value.to_string()))
    }

    /// The id text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VisualizationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One immutable published revision of a visualization.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VisualizationRef {
    /// Visualization id, stable across revisions.
    pub id: VisualizationId,
    /// 1-based revision number.
    pub revision: u32,
}

impl VisualizationRef {
    /// The exact line an assistant writes to place this revision.
    #[must_use]
    pub fn reference_line(&self) -> String {
        format!(
            "{REFERENCE_PREFIX}id=\"{}\" rev=\"{}\"}}",
            self.id, self.revision
        )
    }
}

/// What a complete reference-shaped line resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectiveLine {
    /// A well-formed reference.
    Ready(VisualizationRef),
    /// Starts with the prefix and ends in `}` but does not parse.
    Unavailable,
    /// Starts with the prefix but never closed; hidden.
    Discarded,
}

/// Parse a trimmed directive such as `::lingxi-visualization{id="a" rev="1"}`.
#[must_use]
pub fn parse_directive(directive: &str) -> Option<VisualizationRef> {
    let body = directive
        .strip_prefix(REFERENCE_PREFIX)?
        .strip_suffix('}')?;
    let mut id = None;
    let mut revision = None;
    let mut rest = body.trim_matches(' ');
    while !rest.is_empty() {
        let (key, after) = rest.split_once("=\"")?;
        let (value, after) = after.split_once('"')?;
        match key {
            "id" if id.is_none() => id = Some(VisualizationId::parse(value)?),
            "rev" if revision.is_none() => revision = Some(parse_revision(value)?),
            _ => return None,
        }
        rest = match after.strip_prefix(' ') {
            Some(next) => next.trim_start_matches(' '),
            None if after.is_empty() => after,
            None => return None,
        };
    }
    Some(VisualizationRef {
        id: id?,
        revision: revision?,
    })
}

fn parse_revision(value: &str) -> Option<u32> {
    if value.is_empty() || value.starts_with('0') || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// Number of leading ASCII spaces.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Classify one complete line (no trailing `\n`) that is not code.
#[must_use]
pub fn classify_line(line: &str) -> Option<DirectiveLine> {
    let leading = indent(line);
    if leading > MAX_INDENT {
        return None;
    }
    let rest = &line[leading..];
    if !rest.starts_with(REFERENCE_PREFIX) {
        return None;
    }
    let trimmed = rest.trim_end();
    Some(match parse_directive(trimmed) {
        Some(reference) => DirectiveLine::Ready(reference),
        None if trimmed.ends_with('}') => DirectiveLine::Unavailable,
        None => DirectiveLine::Discarded,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OpenFence {
    marker: u8,
    len: usize,
}

/// `CommonMark` fenced-code tracking for top-level fences (up to three
/// spaces of indentation). Lines inside a fence, and the fence lines
/// themselves, are code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FenceState {
    open: Option<OpenFence>,
}

impl FenceState {
    /// True while a fence is open, so the next line is code.
    #[must_use]
    pub fn in_code(&self) -> bool {
        self.open.is_some()
    }

    /// Feed one complete line; returns true when it is code.
    pub fn observe_line(&mut self, line: &str) -> bool {
        let leading = indent(line);
        if leading <= MAX_INDENT {
            let rest = &line[leading..];
            if let Some(&marker) = rest.as_bytes().first() {
                if marker == b'`' || marker == b'~' {
                    let run = rest.bytes().take_while(|&byte| byte == marker).count();
                    if run >= 3 {
                        let after = &rest[run..];
                        match self.open {
                            None => {
                                // A backtick fence's info string may not contain a backtick.
                                if marker == b'~' || !after.contains('`') {
                                    self.open = Some(OpenFence { marker, len: run });
                                    return true;
                                }
                            }
                            Some(open) => {
                                if marker == open.marker
                                    && run >= open.len
                                    && after.trim().is_empty()
                                {
                                    self.open = None;
                                }
                                return true;
                            }
                        }
                    }
                }
            }
        }
        self.open.is_some()
    }
}

/// The per-text-block line classifier shared by live and replay.
#[derive(Debug, Clone, Default)]
pub struct LineClassifier {
    fence: FenceState,
}

impl LineClassifier {
    /// Classify the next complete line of the block, advancing fence state.
    pub fn classify(&mut self, line: &str) -> Option<DirectiveLine> {
        if self.fence.observe_line(line) {
            return None;
        }
        classify_line(line)
    }

    /// Whether a line whose first bytes are `partial` can still turn out to
    /// be a reference. Once false it stays false for every extension of
    /// `partial`, which is what lets the streaming splitter release text.
    #[must_use]
    pub fn could_be_reference(&self, partial: &str) -> bool {
        if self.fence.in_code() {
            return false;
        }
        let leading = indent(partial);
        if leading > MAX_INDENT {
            return false;
        }
        let rest = &partial[leading..];
        if rest.len() <= REFERENCE_PREFIX.len() {
            REFERENCE_PREFIX.starts_with(rest)
        } else {
            rest.starts_with(REFERENCE_PREFIX)
        }
    }

    /// Whether `partial` already carries the complete prefix.
    #[must_use]
    pub fn has_full_prefix(&self, partial: &str) -> bool {
        self.could_be_reference(partial)
            && partial
                .trim_start_matches(' ')
                .starts_with(REFERENCE_PREFIX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(id: &str, revision: u32) -> VisualizationRef {
        VisualizationRef {
            id: VisualizationId::parse(id).unwrap(),
            revision,
        }
    }

    #[test]
    fn parses_the_canonical_line_and_round_trips() {
        let parsed = parse_directive("::lingxi-visualization{id=\"sales_q3-2\" rev=\"12\"}");
        assert_eq!(parsed, Some(reference("sales_q3-2", 12)));
        assert_eq!(
            reference("a", 1).reference_line(),
            "::lingxi-visualization{id=\"a\" rev=\"1\"}"
        );
        assert_eq!(
            parse_directive(&reference("a", 1).reference_line()),
            Some(reference("a", 1))
        );
    }

    #[test]
    fn accepts_either_attribute_order_and_extra_spaces() {
        assert_eq!(
            parse_directive("::lingxi-visualization{ rev=\"3\"  id=\"x\" }"),
            Some(reference("x", 3))
        );
    }

    #[test]
    fn rejects_malformed_attributes() {
        for directive in [
            "::lingxi-visualization{id=\"a\"}",
            "::lingxi-visualization{rev=\"1\"}",
            "::lingxi-visualization{id=\"a\" rev=\"0\"}",
            "::lingxi-visualization{id=\"a\" rev=\"01\"}",
            "::lingxi-visualization{id=\"a\" rev=\"-1\"}",
            "::lingxi-visualization{id=\"a\" rev=\"99999999999\"}",
            "::lingxi-visualization{id=\"a b\" rev=\"1\"}",
            "::lingxi-visualization{id=\"\" rev=\"1\"}",
            "::lingxi-visualization{id=\"a\" rev=\"1\" extra=\"x\"}",
            "::lingxi-visualization{id=\"a\" id=\"b\" rev=\"1\"}",
            "::lingxi-visualization{id=\"a\"rev=\"1\"}",
            "::lingxi-visualization{id=a rev=1}",
            "::lingxi-visualization{id=\"../x\" rev=\"1\"}",
        ] {
            assert_eq!(parse_directive(directive), None, "{directive}");
        }
        let long = "a".repeat(MAX_ID_LEN + 1);
        assert_eq!(
            parse_directive(&format!(
                "::lingxi-visualization{{id=\"{long}\" rev=\"1\"}}"
            )),
            None
        );
    }

    #[test]
    fn classifies_unavailable_and_discarded_like_upstream() {
        assert_eq!(
            classify_line("::lingxi-visualization{id=\"a\" rev=\"x\"}"),
            Some(DirectiveLine::Unavailable)
        );
        assert_eq!(
            classify_line("::lingxi-visualization{id=\"a\" rev=\"1"),
            Some(DirectiveLine::Discarded)
        );
        assert_eq!(
            classify_line("::lingxi-visualization{id=\"a\" rev=\"1\"} trailing"),
            Some(DirectiveLine::Discarded)
        );
        assert_eq!(
            classify_line("text ::lingxi-visualization{id=\"a\" rev=\"1\"}"),
            None
        );
    }

    #[test]
    fn indentation_and_tabs_follow_commonmark() {
        let line = "::lingxi-visualization{id=\"a\" rev=\"1\"}";
        assert!(matches!(
            classify_line(&format!("   {line}  ")),
            Some(DirectiveLine::Ready(_))
        ));
        assert_eq!(classify_line(&format!("    {line}")), None);
        assert_eq!(classify_line(&format!("\t{line}")), None);
        assert!(matches!(
            classify_line(&format!("{line}\r")),
            Some(DirectiveLine::Ready(_))
        ));
    }

    #[test]
    fn fenced_blocks_keep_directive_literals() {
        let mut classifier = LineClassifier::default();
        let line = "::lingxi-visualization{id=\"a\" rev=\"1\"}";
        assert_eq!(classifier.classify("```html"), None);
        assert_eq!(classifier.classify(line), None);
        assert_eq!(classifier.classify("``"), None);
        assert_eq!(classifier.classify(line), None);
        assert_eq!(classifier.classify("```"), None);
        assert!(matches!(
            classifier.classify(line),
            Some(DirectiveLine::Ready(_))
        ));
        assert_eq!(classifier.classify("~~~~"), None);
        assert_eq!(
            classifier.classify("~~~"),
            None,
            "shorter tilde fence does not close"
        );
        assert_eq!(classifier.classify(line), None);
        assert_eq!(classifier.classify("~~~~~"), None);
        assert!(matches!(
            classifier.classify(line),
            Some(DirectiveLine::Ready(_))
        ));
    }

    #[test]
    fn backtick_info_string_with_backtick_is_not_a_fence() {
        let mut classifier = LineClassifier::default();
        assert_eq!(classifier.classify("``` a`b"), None);
        assert!(!classifier.fence.in_code());
    }

    #[test]
    fn prefix_feasibility_is_monotone() {
        let classifier = LineClassifier::default();
        assert!(classifier.could_be_reference(""));
        assert!(classifier.could_be_reference("   "));
        assert!(classifier.could_be_reference("  ::ling"));
        assert!(classifier.could_be_reference("::lingxi-visualization{id"));
        assert!(!classifier.could_be_reference("    "));
        assert!(!classifier.could_be_reference(":)"));
        assert!(!classifier.could_be_reference("\t"));
        assert!(classifier.has_full_prefix(" ::lingxi-visualization{"));
        assert!(!classifier.has_full_prefix("::lingxi-visualization"));
    }
}
