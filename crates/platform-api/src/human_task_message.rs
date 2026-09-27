//! Parsing shared by typed-human `/tasks message` host entrypoints.
//! This parser grants no authority; only those host entrypoints may call the
//! registry's dedicated human-message method.

/// Parse `message <task-id> <text>` arguments, preserving the text after its
/// first separating whitespace. Other `/tasks` actions return `None`.
pub fn parse(args: &str) -> Option<Result<(&str, &str), &'static str>> {
    let args = args.trim_start();
    let (action, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    if action != "message" {
        return None;
    }
    let rest = rest.trim_start();
    let Some((task_id, message)) = rest.split_once(char::is_whitespace) else {
        return Some(Err("Usage: /tasks message <task-id> <message>"));
    };
    if task_id.is_empty() || message.trim().is_empty() {
        return Some(Err("Usage: /tasks message <task-id> <message>"));
    }
    Some(Ok((task_id, message)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn recognizes_only_message_and_preserves_payload() {
        assert_eq!(
            super::parse("message a123  keep indent\nnext "),
            Some(Ok(("a123", " keep indent\nnext ")))
        );
        assert!(super::parse("").is_none());
        assert!(super::parse("output a123").is_none());
        for invalid in ["message", "message a123", "message a123  "] {
            assert!(super::parse(invalid).unwrap().is_err());
        }
    }
}
