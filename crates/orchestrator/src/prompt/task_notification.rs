//! Shared task notification wire formatter and provider.
pub use lingxi_core::host::task_notification::*;

pub fn render_reminders(
    notifications: &[lingxi_core::host::task_registry::TaskNotification],
) -> Vec<String> {
    render_reminders_in_turn(notifications, false)
}

pub fn render_reminders_in_turn(
    notifications: &[lingxi_core::host::task_registry::TaskNotification],
    in_human_turn: bool,
) -> Vec<String> {
    lingxi_core::host::task_notification::render_reminders_with_options(
        notifications,
        in_human_turn,
        telemetry::push_notifications_enabled(),
    )
}
