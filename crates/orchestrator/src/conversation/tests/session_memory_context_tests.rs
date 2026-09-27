use super::*;

#[test]
fn extraction_context_appends_messages_after_the_cache_safe_prefix() {
    let prefix = ConversationMessage::user(MessageId::new(), "cached prefix".to_string());
    let recent = ConversationMessage::user(MessageId::new(), "recent tool evidence".to_string());
    let history = vec![prefix.clone(), recent];
    let mut fork_context = vec![prefix];

    extend_session_memory_fork_context(&mut fork_context, &history);

    assert_eq!(fork_context, history);
}

#[test]
fn extraction_context_replaces_a_stale_prefix_after_history_rewrite() {
    let mut fork_context = vec![ConversationMessage::user(
        MessageId::new(),
        "pre-compact history".to_string(),
    )];
    let history = vec![ConversationMessage::user(
        MessageId::new(),
        "compact summary".to_string(),
    )];

    extend_session_memory_fork_context(&mut fork_context, &history);

    assert_eq!(fork_context, history);
}
