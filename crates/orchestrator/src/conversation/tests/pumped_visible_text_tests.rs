use super::super::drivers_impl::pumped_has_visible_text;
use lingxi_core::types::ContentBlock;

fn text(s: &str) -> ContentBlock {
    ContentBlock::Text {
        text: s.to_string(), citations: None,
    }
}
fn thinking(s: &str) -> ContentBlock {
    ContentBlock::Thinking {
        thinking: s.to_string(),
        signature: None,
    }
}

#[test]
fn empty_blocks_have_no_visible_text() {
    assert!(!pumped_has_visible_text(&[]));
}

#[test]
fn thinking_only_has_no_visible_text() {
    assert!(!pumped_has_visible_text(&[thinking("reasoning")]));
}

#[test]
fn whitespace_only_text_is_not_visible() {
    assert!(!pumped_has_visible_text(&[text("   \n\t ")]));
}

#[test]
fn non_empty_text_is_visible() {
    assert!(pumped_has_visible_text(&[text("hello")]));
}

#[test]
fn thinking_plus_real_text_is_visible() {
    assert!(pumped_has_visible_text(&[
        thinking("reasoning"),
        text("answer")
    ]));
}
