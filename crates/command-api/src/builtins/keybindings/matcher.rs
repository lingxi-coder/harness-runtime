//! Keystroke matching against live input — a 1:1 port of
//! `claude-code/src/keybindings/match.ts`.
//!
//! claude-code's `match.ts` matches against Ink's `Key` (boolean flags like
//! `key.escape`/`key.return`/`key.upArrow` plus `input`). This crate has no Ink;
//! the live TUI runs on crossterm. To stay dependency-free and reusable from
//! either crossterm version, the caller lowers its native key event into an
//! [`InputKey`] (key NAME already normalized to the `ParsedKeystroke.key`
//! vocabulary, plus the modifier booleans). That mirrors what `getKeyName`
//! produces from Ink, so the matching logic here is byte-faithful.

use super::types::ParsedKeystroke;

/// A normalized input key, the crossterm-agnostic analogue of Ink's `Key` +
/// `input`. Built at the dispatch site from the native event.
///
/// `key` is the already-normalized base name in the same vocabulary as
/// [`ParsedKeystroke::key`] (e.g. `"escape"`, `"enter"`, `"up"`, `"k"`, `" "`).
/// `escape` carries the Ink "key.escape" flag so the escape-meta quirk
/// (match.ts:96-102) can be reproduced.
// Modifier/flag booleans mirror Ink's `Key` (ctrl/shift/meta/super + escape)
// 1:1; the matching logic reads them individually.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InputKey {
    /// Normalized base key name (lowercase single char or named key).
    pub key: String,
    /// Ctrl held.
    pub ctrl: bool,
    /// Shift held.
    pub shift: bool,
    /// Alt/meta held. Terminals report Alt as "meta" — a single boolean here
    /// (the Ink `key.meta`), collapsing alt and meta (see [`modifiers_match`]).
    pub meta: bool,
    /// Super (cmd/win) held — only via the kitty keyboard protocol.
    pub super_: bool,
    /// Whether this is the Escape key (the Ink `key.escape` flag), used to
    /// reproduce the escape-meta quirk in matching.
    pub escape: bool,
}

/// Check whether the input modifiers match a target keystroke's modifiers.
///
/// 1:1 with `modifiersMatch` (match.ts:60-79): ctrl/shift exact; alt and meta
/// both map to the input's `meta` flag (terminal limitation) so the target
/// matches when EITHER `alt` OR `meta` is set; `super` is distinct.
#[must_use]
pub fn modifiers_match(input: &InputKey, target: &ParsedKeystroke) -> bool {
    if input.ctrl != target.ctrl {
        return false;
    }
    if input.shift != target.shift {
        return false;
    }
    let target_needs_meta = target.alt || target.meta;
    if input.meta != target_needs_meta {
        return false;
    }
    if input.super_ != target.super_ {
        return false;
    }
    true
}

/// Check whether a [`ParsedKeystroke`] matches the given input.
///
/// 1:1 with `matchesKeystroke` (match.ts:86-105): the key name must match
/// exactly; then modifiers. QUIRK (match.ts:96-102): Ink sets `key.meta=true`
/// when Escape is pressed, so for the escape key we ignore the meta modifier
/// when matching (otherwise a bare `escape` binding would never fire).
#[must_use]
pub fn matches_keystroke(input: &InputKey, target: &ParsedKeystroke) -> bool {
    if input.key != target.key {
        return false;
    }
    if input.escape {
        let mut no_meta = input.clone();
        no_meta.meta = false;
        return modifiers_match(&no_meta, target);
    }
    modifiers_match(input, target)
}
