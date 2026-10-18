//! Native 2.1.295's one-shot same-model retry after a safety refusal.

use std::ffi::OsStr;

use crate::conversation::ConversationOrchestrator;
use lingxi_core::types::{utf16_json::Utf16JsonProjection, ConversationMessage, MessageId};

const DISABLE_REFUSAL_RETRY_ENV: &str = "CLAUDE_CODE_DISABLE_REFUSAL_RETRY";

pub(crate) const NATIVE_REFUSAL_SAFETY_RETRY_USER_MESSAGE: &str = "Your response above was stopped by a safety classifier — this is not a tool or API error. The rest of it was withheld, and tool calls in it that had not finished did not run. Do not produce that content again, even reworded.";
const NATIVE_REFUSAL_RUNNING_TOOL_EXCEPTION: &str = " Exception: a tool call whose result reads \"Interrupted\" was already running when the response was stopped; it may have partially or fully completed.";

/// Native's `ds(querySource)` classifies only these query sources as
/// non-auxiliary. An absent query source remains `undefined` and therefore
/// passes the `!== "auxiliary"` check.
fn is_non_auxiliary_query_source(query_source: Option<&str>) -> bool {
    let Some(query_source) = query_source else {
        return true;
    };

    query_source.starts_with("repl_main_thread")
        || query_source == "sdk"
        || query_source.starts_with("agent:")
        || query_source == "hook_agent"
}

fn retry_allowed(
    query_source: Option<&str>,
    retry_spent: bool,
    disable_flag: Option<&OsStr>,
) -> bool {
    !retry_spent
        && is_non_auxiliary_query_source(query_source)
        && !disable_flag.is_some_and(|value| !value.is_empty())
}

/// Evaluate Native's `Wa(querySource, refusalRetrySpent)` gate. The one-shot
/// state belongs to the current query; the environment variable uses its raw
/// process value, where any non-empty value disables the retry.
pub(crate) fn native_refusal_retry_enabled(query_source: &str, refusal_retry_spent: bool) -> bool {
    retry_allowed(
        Some(query_source),
        refusal_retry_spent,
        std::env::var_os(DISABLE_REFUSAL_RETRY_ENV).as_deref(),
    )
}

/// Preserve the native model-catalog route fence for same-model retries.
pub(crate) async fn native_refusal_retry_route(
    orch: &ConversationOrchestrator,
    model: &str,
) -> bool {
    let profile = if let Some(route) =
        crate::query_model::current().filter(|route| route.model.as_str() == model)
    {
        route.profile
    } else {
        orch.session.lock().await.model_profile.clone()
    };
    native_refusal_retry_route_for_request(orch, model, profile.as_deref()).await
}

/// Resolve the catalog fence from the model/profile that actually selected
/// this response. Batched Mod responses can report a provider-side model name
/// that differs from the route used for the physical request.
pub(crate) async fn native_refusal_retry_route_for_request(
    orch: &ConversationOrchestrator,
    model: &str,
    profile: Option<&str>,
) -> bool {
    orch.api
        .resolve_media_route(model, profile)
        .ok()
        .is_some_and(|route| ConversationOrchestrator::route_uses_native_model_catalog(&route))
}

/// Emit the Native refusal notice and append its one-shot meta user nudge.
pub(crate) async fn inject_native_refusal_safety_retry(
    orch: &ConversationOrchestrator,
    model: &str,
    refused_message_id: &str,
    has_interrupted_tool: bool,
) {
    let label = crate::prompt::env_meta::marketing_name_for_model(model).unwrap_or(model);
    let notice = format!(
        "{label}'s safeguards stopped the response above · continuing once with that noted"
    );
    orch.output.emit_system_notice(&notice.into(), false).await;

    let user_message = if has_interrupted_tool {
        format!("{NATIVE_REFUSAL_SAFETY_RETRY_USER_MESSAGE}{NATIVE_REFUSAL_RUNNING_TOOL_EXCEPTION}")
    } else {
        NATIVE_REFUSAL_SAFETY_RETRY_USER_MESSAGE.to_owned()
    };
    let mut user = ConversationMessage::user_meta(MessageId::new(), user_message);
    if let ConversationMessage::User {
        turn_companion,
        refused_message_id: carrier,
        ..
    } = &mut user
    {
        *turn_companion = Some(Some(true));
        *carrier = Some(
            Utf16JsonProjection::root_string(
                refused_message_id.to_owned(),
                refused_message_id.encode_utf16().collect(),
            )
            .expect("provider refusal message id is a valid UTF-16 string"),
        );
    }
    orch.session.lock().await.history.push(user.clone());
    orch.persist_message_to_jsonl(&user).await;
    orch.note_query_user_event();
}

#[cfg(test)]
mod tests {
    use super::{retry_allowed, DISABLE_REFUSAL_RETRY_ENV};
    use std::ffi::OsStr;

    #[test]
    fn native_query_source_gate_matches_the_295_classifier() {
        for source in [
            "repl_main_thread",
            "repl_main_thread:fork",
            "sdk",
            "agent:reviewer",
            "agent:custom",
            "hook_agent",
        ] {
            assert!(retry_allowed(Some(source), false, None), "{source:?}");
        }

        for source in ["", "side_question", "compact", "btw", "background", "other"] {
            assert!(!retry_allowed(Some(source), false, None), "{source:?}");
        }

        assert!(retry_allowed(None, false, None));
        assert!(!retry_allowed(Some("sdk"), true, None));
    }

    #[test]
    fn any_raw_nonempty_disable_flag_blocks_the_retry() {
        assert!(!retry_allowed(Some("sdk"), false, Some(OsStr::new("1"))));
        assert!(!retry_allowed(Some("sdk"), false, Some(OsStr::new(" "))));
        assert!(retry_allowed(Some("sdk"), false, Some(OsStr::new(""))));
        assert_eq!(
            DISABLE_REFUSAL_RETRY_ENV,
            "CLAUDE_CODE_DISABLE_REFUSAL_RETRY"
        );
    }
}
