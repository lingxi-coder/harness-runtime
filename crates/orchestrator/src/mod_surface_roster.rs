//! Per-orchestrator remote UI surface attachments exposed to Mods.

use crate::config::ModRenderSurface;
use std::sync::RwLock;

/// One remote renderer attached to a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModSurfaceAttachment {
    /// Stable client identifier supplied by the renderer.
    pub client_id: String,
    /// Surface reported by that renderer.
    pub surface: ModRenderSurface,
}

/// Validation failure when a remote renderer attaches to the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModSurfaceRosterError {
    /// Client ids use the native 1–64 ASCII character contract.
    InvalidClientId,
    /// `terminal` is owned by the host, not a remote renderer.
    HostOwnedSurface,
}

/// Native reason attached to `session.detach` lifecycle events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModSurfaceDetachReason {
    /// A UI client explicitly detached or its transport went away.
    Detach,
    /// The session genuinely ended while the client was still attached.
    End,
}

impl ModSurfaceDetachReason {
    /// Native `session.detach` wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Detach => "detach",
            Self::End => "end",
        }
    }
}

impl std::fmt::Display for ModSurfaceRosterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidClientId => f.write_str("invalid UI client id"),
            Self::HostOwnedSurface => f.write_str("terminal is not a remote UI surface"),
        }
    }
}

impl std::error::Error for ModSurfaceRosterError {}

/// Ordered UI attachments owned by one live orchestrator/session runtime.
/// The roster survives in-place `clear` and `resume`, where the native process
/// keeps its UI clients attached, and is removed by the connection/runtime
/// teardown path one owned client at a time.
#[derive(Debug, Default)]
pub struct ModSurfaceRoster {
    attachments: RwLock<Vec<ModSurfaceAttachment>>,
}

impl ModSurfaceRoster {
    /// Add a remote UI client. Duplicate client ids are idempotent and do not
    /// change their original surface or insertion order.
    pub fn attach(
        &self,
        client_id: impl Into<String>,
        surface: ModRenderSurface,
    ) -> Result<bool, ModSurfaceRosterError> {
        self.attach_owned(client_id.into(), surface, false)
    }

    /// Admit the engine-owned renderer used when ui_render omits client_id.
    pub fn attach_default(&self, surface: ModRenderSurface) -> Result<bool, ModSurfaceRosterError> {
        self.attach_owned(format!("{}:default", surface.as_str()), surface, true)
    }

    fn attach_owned(
        &self,
        client_id: String,
        surface: ModRenderSurface,
        engine_default: bool,
    ) -> Result<bool, ModSurfaceRosterError> {
        if !valid_client_id(&client_id) && !engine_default {
            return Err(ModSurfaceRosterError::InvalidClientId);
        }
        if surface == ModRenderSurface::Terminal {
            return Err(ModSurfaceRosterError::HostOwnedSurface);
        }

        let mut attachments = self
            .attachments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if attachments.iter().any(|item| item.client_id == client_id) {
            return Ok(false);
        }
        attachments.push(ModSurfaceAttachment { client_id, surface });
        Ok(true)
    }

    /// Remove one known client and return its last attached surface.
    pub fn detach(&self, client_id: &str) -> Option<ModSurfaceAttachment> {
        let mut attachments = self
            .attachments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = attachments
            .iter()
            .position(|item| item.client_id == client_id)?;
        Some(attachments.remove(index))
    }

    /// Snapshot unique remote surfaces in first-attachment order.
    #[must_use]
    pub fn surfaces(&self) -> Vec<ModRenderSurface> {
        let attachments = self
            .attachments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut surfaces = Vec::new();
        for item in attachments.iter() {
            if !surfaces.contains(&item.surface) {
                surfaces.push(item.surface);
            }
        }
        surfaces
    }

    /// Snapshot all clients in attachment order.
    #[must_use]
    pub fn attachments(&self) -> Vec<ModSurfaceAttachment> {
        self.attachments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// True when the specified client is attached.
    #[must_use]
    pub fn contains(&self, client_id: &str) -> bool {
        self.attachments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|item| item.client_id == client_id)
    }
}

/// Native `clientId` accepts 1–64 ASCII characters from this exact set.
#[must_use]
pub fn valid_client_id(client_id: &str) -> bool {
    !client_id.is_empty()
        && client_id.len() <= 64
        && client_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_default_client_does_not_expand_external_id_contract() {
        let roster = ModSurfaceRoster::default();
        assert!(roster
            .attach("mobile:default", ModRenderSurface::Mobile)
            .is_err());
        assert!(roster.attach_default(ModRenderSurface::Mobile).unwrap());
        assert!(!roster.attach_default(ModRenderSurface::Mobile).unwrap());
        assert_eq!(roster.attachments()[0].client_id, "mobile:default");
        assert_eq!(roster.surfaces(), vec![ModRenderSurface::Mobile]);
        assert!(roster.attach_default(ModRenderSurface::Terminal).is_err());
    }

    #[test]
    fn duplicate_attach_is_idempotent_and_keeps_first_surface() {
        let roster = ModSurfaceRoster::default();

        assert!(roster
            .attach("window-a", ModRenderSurface::Desktop)
            .unwrap());
        assert!(!roster.attach("window-a", ModRenderSurface::Vscode).unwrap());
        assert_eq!(roster.surfaces(), vec![ModRenderSurface::Desktop]);
        assert_eq!(roster.attachments().len(), 1);
    }

    #[test]
    fn same_surface_remains_until_its_last_client_detaches() {
        let roster = ModSurfaceRoster::default();
        roster
            .attach("window-a", ModRenderSurface::Desktop)
            .unwrap();
        roster
            .attach("window-b", ModRenderSurface::Desktop)
            .unwrap();

        assert_eq!(roster.surfaces(), vec![ModRenderSurface::Desktop]);
        assert_eq!(roster.detach("unknown"), None);
        assert_eq!(roster.detach("window-a").unwrap().client_id, "window-a");
        assert_eq!(roster.surfaces(), vec![ModRenderSurface::Desktop]);
        assert_eq!(roster.detach("window-b").unwrap().client_id, "window-b");
        assert!(roster.surfaces().is_empty());
    }

    #[test]
    fn surfaces_preserve_first_attachment_order() {
        let roster = ModSurfaceRoster::default();
        roster.attach("window-a", ModRenderSurface::Mobile).unwrap();
        roster
            .attach("window-b", ModRenderSurface::Desktop)
            .unwrap();
        roster.attach("window-c", ModRenderSurface::Mobile).unwrap();

        assert_eq!(
            roster.surfaces(),
            vec![ModRenderSurface::Mobile, ModRenderSurface::Desktop]
        );
    }

    #[test]
    fn client_ids_match_native_bounds_and_character_set() {
        assert!(!valid_client_id(""));
        assert!(valid_client_id("a"));
        assert!(valid_client_id(&"a".repeat(64)));
        assert!(!valid_client_id(&"a".repeat(65)));
        assert!(!valid_client_id("window/1"));
        assert!(!valid_client_id("window 1"));
    }

    #[test]
    fn terminal_cannot_be_announced_as_remote_attachment() {
        let roster = ModSurfaceRoster::default();
        assert_eq!(
            roster.attach("window-a", ModRenderSurface::Terminal),
            Err(ModSurfaceRosterError::HostOwnedSurface)
        );
    }
}
