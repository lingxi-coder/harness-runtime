use super::super::drivers_impl::pumped_has_visible_text;
use lingxi_core::types::ContentBlock;

fn text(s: &str) -> ContentBlock {
    ContentBlock::Text {
        text: s.to_string(),
        citations: None,
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

#[test]
fn preserved_native_text_is_visible_but_opaque_non_text_and_blank_are_not() {
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    let block = |protocol: &str, kind: &str, text: &str| ContentBlock::ProviderContent {
        protocol: protocol.into(),
        value: Utf16JsonProjection::from_serde_value(&serde_json::json!({
            "type":kind,"text":text,"provider_extra":{"preserved":true}
        }))
        .unwrap(),
    };
    assert!(pumped_has_visible_text(&[block(
        "anthropic_messages",
        "text",
        "answer"
    )]));
    assert!(!pumped_has_visible_text(&[block(
        "anthropic_messages",
        "text",
        "  \n"
    )]));
    assert!(!pumped_has_visible_text(&[block(
        "anthropic_messages",
        "thinking",
        "private reasoning"
    )]));
    assert!(!pumped_has_visible_text(&[block(
        "unknown_protocol",
        "text",
        "opaque data"
    )]));
}

#[test]
fn display_text_with_exact_js_utf16_is_visible() {
    assert!(pumped_has_visible_text(&[ContentBlock::TextJsUtf16 {
        text: "\u{fffd}".into(),
        utf16_code_units: vec![0xd800],
        citations: None,
    }]));
    assert!(!pumped_has_visible_text(&[ContentBlock::TextJsUtf16 {
        text: " \t".into(),
        utf16_code_units: vec![0x20, 0x9],
        citations: None,
    }]));
}
