//! Shared task notification wire formatter and provider.
pub use lingxi_core::host::task_notification::*;
use lingxi_core::types::utf16_json::Utf16JsonProjection;

pub fn render_reminders(
    notifications: &[lingxi_core::host::task_registry::TaskNotification],
) -> Vec<Utf16JsonProjection> {
    render_reminders_in_turn(notifications, false)
}

pub fn render_reminders_in_turn(
    notifications: &[lingxi_core::host::task_registry::TaskNotification],
    in_human_turn: bool,
) -> Vec<Utf16JsonProjection> {
    lingxi_core::host::task_notification::render_reminders_with_options(
        notifications,
        in_human_turn,
        telemetry::push_notifications_enabled(),
    )
}

/// Produce the exact raw attachment prompt and its model-facing wrapper from
/// the same notification and push-feature snapshot.
pub fn render_reminder_sources_in_turn(
    notifications: &[lingxi_core::host::task_registry::TaskNotification],
    in_human_turn: bool,
) -> Vec<(Utf16JsonProjection, Utf16JsonProjection)> {
    lingxi_core::host::task_notification::render_reminder_sources_with_options(
        notifications,
        in_human_turn,
        telemetry::push_notifications_enabled(),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn notification_headers_match_319_native_source() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/agent_reminders_319.json"
        ))
        .unwrap();
        assert_eq!(
            super::NON_USER_INPUT_HEADER,
            fixture["notificationHeaders"]["normal"].as_str().unwrap()
        );
        assert_eq!(
            super::IN_HUMAN_TURN_HEADER,
            fixture["notificationHeaders"]["inHumanTurn"]
                .as_str()
                .unwrap()
        );
    }
}
