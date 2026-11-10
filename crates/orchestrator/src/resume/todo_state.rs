//! Restore session checklists only from successful TodoWrite history.

use lingxi_core::types::{ContentBlock, ConversationMessage, ToolUseId};
use lingxi_core::{SessionState, TodoItem, TodoState};
use std::collections::HashMap;

pub(crate) fn reset(session: &mut SessionState) {
    session.todos.clear();
    session.turns_since_last_todo_write = 0;
    session.turns_since_last_reminder = 0;
}

pub(crate) fn restore(session: &mut SessionState) {
    reset(session);
    let mut pending: HashMap<ToolUseId, Option<Vec<TodoItem>>> = HashMap::new();
    for message in &session.history {
        match message {
            ConversationMessage::Assistant { content, .. } => {
                for block in content {
                    let ContentBlock::ToolUse {
                        id, name, input, ..
                    } = block
                    else {
                        continue;
                    };
                    if name != "TodoWrite" {
                        continue;
                    }
                    let todos = input
                        .subprojection("/todos")
                        .ok()
                        .and_then(|value| value.to_json_string().ok())
                        .and_then(|value| serde_json::from_str::<Vec<TodoItem>>(&value).ok())
                        .filter(|todos| tool_task::todo_write::validate_todos(todos).is_ok());
                    pending.insert(id.clone(), todos);
                }
            }
            ConversationMessage::User { content, .. } => {
                for block in content {
                    let ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        ..
                    } = block
                    else {
                        continue;
                    };
                    let Some(todos) = pending.remove(tool_use_id) else {
                        continue;
                    };
                    if *is_error == Some(true) {
                        continue;
                    }
                    // A malformed successful replacement cannot keep an older
                    // checklist looking current. An all-completed write has
                    // the same clearing semantics as the live TodoWrite tool.
                    session.todos = todos.unwrap_or_default();
                    if session
                        .todos
                        .iter()
                        .all(|todo| todo.status == TodoState::Completed)
                    {
                        session.todos.clear();
                    }
                    for todo in &mut session.todos {
                        todo.id.clear();
                    }
                }
            }
            ConversationMessage::System { .. } => {}
        }
    }
    // Reminder messages are transient and their exact cadence is not a
    // durable fact. Start a fresh cadence for the newly mounted history.
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::{MessageId, SessionId};

    fn append_write(session: &mut SessionState, todos: &str, error: bool) {
        let id = ToolUseId::new();
        session.history.push(ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::ToolUse {
                id: id.clone(),
                name: "TodoWrite".into(),
                input: lingxi_core::types::utf16_json::Utf16JsonProjection::parse(&format!(
                    r#"{{"todos":{todos}}}"#
                ))
                .unwrap(),
                provider_id: None,
            }],
            stop_reason: Some("tool_use".into()),
            resumable: None,
            per_turn_effort: None,
        });
        let mut result = ConversationMessage::user(MessageId::new(), String::new());
        if let ConversationMessage::User { content, .. } = &mut result {
            *content = vec![ContentBlock::ToolResult {
                tool_use_id: id,
                content: "done".into(),
                is_error: Some(error),
                provider_tool_use_id: None,
                content_blocks: None,
            }];
        }
        session.history.push(result);
    }

    #[test]
    fn restores_only_successful_writes_with_exact_source_and_resets_cadence() {
        let mut session = SessionState::empty(SessionId::new(), "test".into());
        append_write(
            &mut session,
            r#"[{"content":"retained \ud800","status":"pending","activeForm":"Working"}]"#,
            false,
        );
        append_write(
            &mut session,
            r#"[{"content":"failed","status":"pending","activeForm":"Working"}]"#,
            true,
        );
        session.turns_since_last_todo_write = 99;
        session.turns_since_last_reminder = 99;
        restore(&mut session);
        assert_eq!(session.todos.len(), 1);
        assert_eq!(
            session.todos[0].content.string_units(""),
            Some("retained ".encode_utf16().chain([0xd800]).collect())
        );
        assert_eq!(session.turns_since_last_todo_write, 0);
        assert_eq!(session.turns_since_last_reminder, 0);
        append_write(
            &mut session,
            r#"[{"content":"done","status":"completed","activeForm":"Working"}]"#,
            false,
        );
        restore(&mut session);
        assert!(session.todos.is_empty());
    }
}
