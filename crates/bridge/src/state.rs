//! Lightweight bridge-session snapshot. Held by the engine so UI / telemetry
//! can observe whether the local IDE peer is connected and which file (if
//! any) is focused. Two fields only — M2-02 may add more.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Observable session-state snapshot of the local IDE bridge.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeState {
    /// `true` when at least one trusted IDE is connected.
    pub connected: bool,
    /// Path of the file currently focused in the IDE, if known.
    #[serde(default)]
    pub current_file: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disconnected_no_file() {
        let s = BridgeState::default();
        assert!(!s.connected);
        assert!(s.current_file.is_none());
    }

    #[test]
    fn roundtrip_json_preserves_fields() {
        let s = BridgeState {
            connected: true,
            current_file: Some(PathBuf::from("/tmp/foo.rs")),
        };
        let back: BridgeState = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(s.connected, back.connected);
        assert_eq!(s.current_file, back.current_file);
    }
}
