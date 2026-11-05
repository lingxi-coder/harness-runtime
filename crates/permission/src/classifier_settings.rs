//! Exact AutoMode settings merged only from sources admitted by the host.

/// Merge admitted settings tiers in priority order while preserving UTF-16 strings.
/// Returns `None` if a tier cannot be represented by the trusted settings schema.
pub fn merge_trusted_auto_mode_settings(
    trusted_raw_tiers: &[String],
) -> Option<lingxi_core::host::permission_gate::PermissionClassifierAutoModeSettings> {
    use lingxi_core::host::permission_gate::PermissionClassifierAutoModeSettings;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    let mut merged = PermissionClassifierAutoModeSettings::default();
    for raw in trusted_raw_tiers {
        let source = Utf16JsonProjection::parse(raw).ok()?;
        let Some(auto_mode) = source.get_property("autoMode") else {
            continue;
        };
        if auto_mode.object_entries().is_none() {
            return None;
        }
        let auto_mode = source.subprojection("/autoMode").ok()?;
        for (key, destination) in [
            ("allow", &mut merged.allow),
            ("soft_deny", &mut merged.soft_deny),
            ("hard_deny", &mut merged.hard_deny),
            ("environment", &mut merged.environment),
        ] {
            let Some(values) = auto_mode.get_property(key) else {
                continue;
            };
            let count = values.array_len()?;
            for index in 0..count {
                let value = values.get_index(index)?;
                if value.code_units().is_none() {
                    return None;
                }
                destination.push(auto_mode.subprojection(&format!("/{key}/{index}")).ok()?);
            }
        }
        if let Some(value) = auto_mode.get_property("classifyAllShell") {
            merged.classify_all_shell |= value.as_bool()?;
        }
    }
    Some(merged)
}
