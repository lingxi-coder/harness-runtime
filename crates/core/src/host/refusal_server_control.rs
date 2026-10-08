//! Pure server refusal-fallback effects and received-model identity decisions.

use super::refusal_state::wire_identity;

/// Exact native error returned when a server-selected fallback is unavailable.
pub const SERVER_FALLBACK_ALLOWLIST_ERROR: &str = "The server routed this response to a model that is not in your organization’s availableModels allowlist; the response was discarded.";

/// The scope selected by the native fallback banner decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerScope {
    /// The query belongs to the main thread and swaps its session model.
    Session,
    /// The event is local to a child query, or no banner will be emitted.
    Local,
}

/// Facts needed to decide the UI and session effects of a server fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Input<'a> {
    /// Native event reason; only `refusal` and `sticky` are user-visible.
    pub reason: &'a str,
    /// Whether any discarded message contains a `tool_use` block.
    pub discarded_had_tool_use: bool,
    /// Whether this event belongs to the main query thread.
    pub is_main_thread: bool,
    /// Whether the child query is allowed to emit a local-scope notice.
    pub emits_local_scope: bool,
}

/// Decisions for applying a server fallback to query/session/UI state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effects {
    /// Whether the fallback is visible to the user.
    pub user_visible: bool,
    /// Whether discarded history included a tool-use block.
    pub tombstoned_tool_use: bool,
    /// Whether the main session model should be swapped.
    pub swap_session: bool,
    /// Whether a fallback banner should be shown.
    pub show_banner: bool,
    /// Native banner scope, including `Local` when `show_banner` is false.
    pub banner_scope: BannerScope,
}

/// Apply the native `oHo` server-fallback control decision to supplied facts.
pub fn control(input: Input<'_>) -> Effects {
    let user_visible = matches!(input.reason, "refusal" | "sticky");
    let swap_session = user_visible && input.is_main_thread;
    let local_notice = user_visible && !input.is_main_thread && input.emits_local_scope;

    Effects {
        user_visible,
        tombstoned_tool_use: input.discarded_had_tool_use,
        swap_session,
        show_banner: swap_session || local_notice,
        banner_scope: if swap_session {
            BannerScope::Session
        } else {
            BannerScope::Local
        },
    }
}

/// Prefer the declared model spelling when native wire identities match.
pub fn resolve_received_model(declared: Option<&str>, received: &str) -> String {
    match declared {
        Some(declared) if wire_identity(declared) == wire_identity(received) => declared.into(),
        _ => received.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/server_fallback_control_2_1_288.json"
        ))
        .unwrap()
    }

    #[test]
    fn native_control_and_declared_model_preference_match() {
        let fixture = fixture();
        assert_eq!(
            SERVER_FALLBACK_ALLOWLIST_ERROR,
            fixture["unavailableAllowlistError"].as_str().unwrap()
        );

        for case in fixture["controlCases"].as_array().unwrap() {
            let effects = control(Input {
                reason: case["input"]["reason"].as_str().unwrap(),
                discarded_had_tool_use: case["input"]["discardedHadToolUse"].as_bool().unwrap(),
                is_main_thread: case["input"]["isMainThread"].as_bool().unwrap(),
                emits_local_scope: case["input"]["emitsLocalScope"].as_bool().unwrap(),
            });
            let actual = json!({
                "userVisible": effects.user_visible,
                "tombstonedToolUse": effects.tombstoned_tool_use,
                "swapSession": effects.swap_session,
                "showBanner": effects.show_banner,
                "bannerScope": match effects.banner_scope {
                    BannerScope::Session => "session",
                    BannerScope::Local => "local",
                },
            });
            assert_eq!(actual, case["expected"], "{case}");
        }

        for case in fixture["receivedModelCases"].as_array().unwrap() {
            let declared = case["declared"].as_str();
            let received = case["received"].as_str().unwrap();
            assert_eq!(
                resolve_received_model(declared, received),
                case["expected"].as_str().unwrap(),
                "{case}"
            );
        }
    }
}
