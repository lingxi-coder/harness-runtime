//! Runtime keymap — the object the TUI consults instead of a hardcoded `match`.
//!
//! There is no single claude-code file for this: it composes the loader output
//! (`KeybindingsLoadResult.bindings`) with the chord-state machinery from
//! `resolver.ts` (`resolveKeyWithChordState` + the React `pending` state held in
//! `KeybindingContext.tsx`). [`Keymap`] holds the flattened bindings; the caller
//! threads a [`PendingChord`] across keystrokes (the analogue of the
//! `pendingChord` React ref) so multi-keystroke chords like `ctrl+x ctrl+k`
//! resolve only after the full sequence.

use super::loader::{load_keybindings, KeybindingsLoadResult};
use super::resolver::{resolve_key_with_chord_state, ChordResolveResult};
use super::types::{ParsedBinding, ParsedKeystroke};
use std::path::Path;

pub use super::matcher::InputKey;

/// The pending-chord state, threaded across keystrokes. `None` = not mid-chord.
/// Mirrors the `pendingChord` ref in `KeybindingContext.tsx`.
pub type PendingChord = Option<Vec<ParsedKeystroke>>;

/// A runtime keymap: the merged (default + user) bindings, ready to resolve.
#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: Vec<ParsedBinding>,
}

/// What a [`Keymap::resolve`] produced. Distinguishes a concrete action from the
/// chord-in-progress and unbind cases so the caller can decide whether to fall
/// through to its legacy hardcoded dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// A binding fully matched; the action id (e.g. `"app:redraw"`).
    Action(String),
    /// The keystroke began/extended a chord prefix; await more keys (consume).
    ChordPending,
    /// A binding matched but is explicitly unbound (`null`); consume, do nothing.
    Unbound,
    /// No binding matched. The caller should fall through to its default path.
    None,
}

impl Keymap {
    /// Build a keymap from a [`KeybindingsLoadResult`] (or any binding list).
    #[must_use]
    pub fn from_load_result(result: KeybindingsLoadResult) -> Self {
        Self {
            bindings: result.bindings,
        }
    }

    /// Build directly from a flat binding list (used by tests / direct callers).
    #[must_use]
    pub fn from_bindings(bindings: Vec<ParsedBinding>) -> Self {
        Self { bindings }
    }

    /// Build the keymap by loading the user config (or defaults). Convenience
    /// for the composition root: `enabled` is the customization gate, `path` the
    /// `keybindings.json` path, `is_macos` the reserved-shortcut platform.
    #[must_use]
    pub fn load(enabled: bool, path: &Path, is_macos: bool) -> Self {
        Self::from_load_result(load_keybindings(enabled, path, is_macos))
    }

    /// The default keymap (no user config) — byte-identical to the hardcoded
    /// defaults. Equivalent to the gate-off / no-file load path.
    #[must_use]
    pub fn defaults() -> Self {
        Self::from_bindings(super::parser::parse_bindings(
            &super::default_bindings::default_bindings(),
        ))
    }

    /// The flattened bindings (for `get_binding_display_text` / help rendering).
    #[must_use]
    pub fn bindings(&self) -> &[ParsedBinding] {
        &self.bindings
    }

    /// Resolve an input key against the active contexts, threading `pending`
    /// chord state. Updates `pending` in place: starts/extends a chord, clears
    /// it on a final match/cancel.
    ///
    /// Returns [`Resolution::None`] when nothing matched — the caller then falls
    /// through to its existing hardcoded dispatch, which is what guarantees zero
    /// behavior change for unmapped actions and for users with no config.
    pub fn resolve(
        &self,
        input: &InputKey,
        active_contexts: &[String],
        pending: &mut PendingChord,
    ) -> Resolution {
        let result = resolve_key_with_chord_state(
            input,
            active_contexts,
            &self.bindings,
            pending.as_deref(),
        );
        match result {
            ChordResolveResult::Match(action) => {
                *pending = None;
                Resolution::Action(action)
            }
            ChordResolveResult::Unbound => {
                *pending = None;
                Resolution::Unbound
            }
            ChordResolveResult::ChordStarted(seq) => {
                *pending = Some(seq);
                Resolution::ChordPending
            }
            ChordResolveResult::ChordCancelled => {
                *pending = None;
                Resolution::None
            }
            ChordResolveResult::None => Resolution::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(key: &str, ctrl: bool) -> InputKey {
        InputKey {
            key: key.to_string(),
            ctrl,
            shift: false,
            meta: false,
            super_: false,
            escape: key == "escape",
        }
    }

    fn global() -> Vec<String> {
        vec!["Global".to_string()]
    }

    // (a) Override resolves + unspecified falls back to defaults.
    #[test]
    fn override_resolves_and_others_fall_back_to_defaults() {
        // Default: ctrl+l → app:redraw, ctrl+o → app:toggleTranscript (Global).
        // User overrides ctrl+l → app:toggleTodos; ctrl+o untouched.
        let json = r#"{ "bindings": [ { "context": "Global", "bindings": { "ctrl+l": "app:toggleTodos" } } ] }"#;
        let p = {
            use std::io::Write;
            let p = std::env::temp_dir().join(format!(
                "lingxi-kb-keymap-override-{}.json",
                std::process::id()
            ));
            std::fs::File::create(&p)
                .unwrap()
                .write_all(json.as_bytes())
                .unwrap();
            p
        };
        let km = Keymap::load(true, &p, false);
        let mut pending: PendingChord = None;

        // Overridden chord → new action.
        assert_eq!(
            km.resolve(&input("l", true), &global(), &mut pending),
            Resolution::Action("app:toggleTodos".to_string())
        );
        // Unspecified default chord still resolves to its default.
        assert_eq!(
            km.resolve(&input("o", true), &global(), &mut pending),
            Resolution::Action("app:toggleTranscript".to_string())
        );
        let _ = std::fs::remove_file(&p);
    }

    // (c) No config ⇒ defaults unchanged.
    #[test]
    fn defaults_keymap_resolves_default_chords() {
        let km = Keymap::defaults();
        let mut pending: PendingChord = None;
        assert_eq!(
            km.resolve(&input("l", true), &global(), &mut pending),
            Resolution::Action("app:redraw".to_string())
        );
        // A chord that doesn't exist in Global → None (fall through).
        assert_eq!(
            km.resolve(&input("z", true), &global(), &mut pending),
            Resolution::None
        );
    }

    // Chord: ctrl+x ctrl+k → chat:killAgents only after both keystrokes.
    #[test]
    fn two_step_chord_pends_then_matches() {
        let km = Keymap::defaults();
        let chat = vec!["Chat".to_string()];
        let mut pending: PendingChord = None;
        // ctrl+x alone is a prefix → ChordPending.
        assert_eq!(
            km.resolve(&input("x", true), &chat, &mut pending),
            Resolution::ChordPending
        );
        assert!(pending.is_some());
        // ctrl+k completes the chord.
        assert_eq!(
            km.resolve(&input("k", true), &chat, &mut pending),
            Resolution::Action("chat:killAgents".to_string())
        );
        assert!(pending.is_none());
    }
}
