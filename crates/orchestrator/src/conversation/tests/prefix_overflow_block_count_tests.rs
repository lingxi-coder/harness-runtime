use super::count_document_and_image_blocks;
use protocol::{ContentBlock, ConversationMessage, DocumentSource, ImageSource, MessageId};

fn image_block() -> ContentBlock {
    ContentBlock::Image {
        source: ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        },
    }
}

fn document_block() -> ContentBlock {
    ContentBlock::Document {
        source: DocumentSource::Base64 {
            media_type: "application/pdf".into(),
            data: "AAAA".into(),
        },
    }
}

#[test]
fn counts_documents_and_images_across_user_and_assistant() {
    // #55 a3p documentBlockCount / imageBlockCount.
    let msgs = vec![
        ConversationMessage::User {
            id: MessageId::new(),
            content: vec![
                ContentBlock::Text { text: "hi".into() },
                image_block(),
                document_block(),
            ],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        },
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![image_block()],
            stop_reason: None,
        },
        // System messages carry a flat string — never counted.
        ConversationMessage::System {
            id: MessageId::new(),
            content: "system".into(),
            subtype: None,
            compact_metadata: None,
            refusal_fallback: None,
        },
    ];
    let (docs, imgs) = count_document_and_image_blocks(&msgs);
    assert_eq!(docs, 1, "one document block across the messages");
    assert_eq!(imgs, 2, "two image blocks across the messages");
}

#[test]
fn counts_zero_when_no_media_blocks() {
    let msgs = vec![ConversationMessage::user(
        MessageId::new(),
        "plain text".into(),
    )];
    assert_eq!(count_document_and_image_blocks(&msgs), (0, 0));
}
