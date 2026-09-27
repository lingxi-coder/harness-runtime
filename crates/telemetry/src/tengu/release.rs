//! Release-marker events. Emitted once at the first successful
//! `Engine::init()` after each tagged release. The constant lives here
//! (one event per release) so version-bump audits have a single grep target.

/// v0.5.0 release marker — emitted once on first init after the upgrade.
/// Wire string locked: appears byte-for-byte in test assertions + the
/// downstream Statsig analytics schema. Actual emit wires in M5 alongside
/// the `Engine::init()` boot path; M4-09 only registers the constant so
/// downstream sinks reserve the slot.
pub const LINGXI_CORE_V0_5_0_RELEASED: &str = "lingxi_core_v0_5_0_released";

/// v0.6.0 release marker — emitted once on first init after the M5 upgrade.
/// Wire string locked: appears byte-for-byte in test assertions + downstream
/// Statsig analytics schema. M5-14 Task 10 emits it from `Engine::init()`.
pub const LINGXI_CORE_V0_6_0_RELEASED: &str = "lingxi_core_v0_6_0_released";

/// v0.7.0 release marker — emitted once on first init after the M6 upgrade.
/// Wire string locked: appears byte-for-byte in test assertions + downstream
/// Statsig analytics schema. M6-09 Task 12 emits it from
/// `ConversationOrchestrator::new` via `std::sync::Once`.
pub const LINGXI_CORE_V0_7_0_RELEASED: &str = "lingxi_core_v0_7_0_released";

/// v0.8.0 release marker — emitted once on first init after the M7 upgrade.
/// Wire string locked: appears byte-for-byte in test assertions + downstream
/// Statsig analytics schema. M7-16 emits it from
/// `ConversationOrchestrator::new` via `std::sync::Once`.
pub const LINGXI_CORE_V0_8_0_RELEASED: &str = "lingxi_core_v0_8_0_released";

/// Order-locked array of all release-marker names; consumed by
/// `tengu::ALL_EVENT_NAMES`. Append-only: never reorder or remove entries.
pub(crate) const NAMES: &[&str] = &[
    LINGXI_CORE_V0_5_0_RELEASED,
    LINGXI_CORE_V0_6_0_RELEASED,
    LINGXI_CORE_V0_7_0_RELEASED,
    LINGXI_CORE_V0_8_0_RELEASED,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v0_6_0_constant_locked() {
        assert_eq!(LINGXI_CORE_V0_6_0_RELEASED, "lingxi_core_v0_6_0_released");
    }

    #[test]
    fn v0_5_0_constant_unchanged() {
        assert_eq!(LINGXI_CORE_V0_5_0_RELEASED, "lingxi_core_v0_5_0_released");
    }

    #[test]
    fn v0_7_0_constant_locked() {
        assert_eq!(LINGXI_CORE_V0_7_0_RELEASED, "lingxi_core_v0_7_0_released");
    }

    #[test]
    fn v0_8_0_constant_locked() {
        assert_eq!(LINGXI_CORE_V0_8_0_RELEASED, "lingxi_core_v0_8_0_released");
    }

    #[test]
    fn names_slice_length_4() {
        assert_eq!(NAMES.len(), 4);
    }

    #[test]
    fn names_slice_order_append_only() {
        // Newer releases MUST be appended; never reorder.
        assert_eq!(NAMES[0], LINGXI_CORE_V0_5_0_RELEASED);
        assert_eq!(NAMES[1], LINGXI_CORE_V0_6_0_RELEASED);
        assert_eq!(NAMES[2], LINGXI_CORE_V0_7_0_RELEASED);
        assert_eq!(NAMES[3], LINGXI_CORE_V0_8_0_RELEASED);
    }
}
