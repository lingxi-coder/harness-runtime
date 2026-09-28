//! Chord-aware key resolution — a 1:1 port of
//! `claude-code/src/keybindings/resolver.ts`.

use super::matcher::{matches_keystroke, InputKey};
use super::parser::chord_to_string;
use super::types::{ParsedBinding, ParsedKeystroke};
use std::collections::{HashMap, HashSet};

/// Result of chord-aware resolution.
/// 1:1 with the TS `ChordResolveResult` (resolver.ts:15-21).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChordResolveResult {
    /// A binding matched; its action id.
    Match(String),
    /// No binding matched and no chord is pending.
    None,
    /// A binding matched but its action is `null` (an explicit unbind).
    Unbound,
    /// This keystroke began (or extended) a chord prefix; the pending sequence.
    ChordStarted(Vec<ParsedKeystroke>),
    /// A pending chord was cancelled (no continuation matched, or Escape).
    ChordCancelled,
}

/// Compare two keystrokes for equality. Collapses alt/meta into one logical
/// modifier (terminals can't distinguish them); super is distinct.
/// 1:1 with `keystrokesEqual` (resolver.ts:107-118).
#[must_use]
pub fn keystrokes_equal(a: &ParsedKeystroke, b: &ParsedKeystroke) -> bool {
    a.key == b.key
        && a.ctrl == b.ctrl
        && a.shift == b.shift
        && (a.alt || a.meta) == (b.alt || b.meta)
        && a.super_ == b.super_
}

/// Build a [`ParsedKeystroke`] from an [`InputKey`].
/// 1:1 with `buildKeystroke` (resolver.ts:82-99): for the escape key, the
/// terminal's spurious meta flag is dropped so chord matching works.
fn build_keystroke(input: &InputKey) -> Option<ParsedKeystroke> {
    if input.key.is_empty() {
        return None;
    }
    let effective_meta = if input.escape { false } else { input.meta };
    Some(ParsedKeystroke {
        key: input.key.clone(),
        ctrl: input.ctrl,
        alt: effective_meta,
        shift: input.shift,
        meta: effective_meta,
        super_: input.super_,
    })
}

/// Whether a chord prefix matches the beginning of a binding's chord.
/// 1:1 with `chordPrefixMatches` (resolver.ts:123-135).
fn chord_prefix_matches(prefix: &[ParsedKeystroke], binding: &ParsedBinding) -> bool {
    if prefix.len() >= binding.chord.len() {
        return false;
    }
    for (i, p) in prefix.iter().enumerate() {
        match binding.chord.get(i) {
            Some(b) if keystrokes_equal(p, b) => {}
            _ => return false,
        }
    }
    true
}

/// Whether a full chord exactly matches a binding's chord.
/// 1:1 with `chordExactlyMatches` (resolver.ts:140-152).
fn chord_exactly_matches(chord: &[ParsedKeystroke], binding: &ParsedBinding) -> bool {
    if chord.len() != binding.chord.len() {
        return false;
    }
    for (i, c) in chord.iter().enumerate() {
        match binding.chord.get(i) {
            Some(b) if keystrokes_equal(c, b) => {}
            _ => return false,
        }
    }
    true
}

/// Resolve a key with chord-state support.
///
/// 1:1 with `resolveKeyWithChordState` (resolver.ts:166-244): handles
/// multi-keystroke chords (`ctrl+x ctrl+k`). `pending` is `None` when not in a
/// chord; the prefix-vs-exact + null-shadows-prefix + last-wins semantics are
/// preserved exactly.
#[must_use]
pub fn resolve_key_with_chord_state(
    input: &InputKey,
    active_contexts: &[String],
    bindings: &[ParsedBinding],
    pending: Option<&[ParsedKeystroke]>,
) -> ChordResolveResult {
    // Cancel chord on escape.
    if input.escape && pending.is_some() {
        return ChordResolveResult::ChordCancelled;
    }

    // Build current keystroke.
    let Some(current) = build_keystroke(input) else {
        if pending.is_some() {
            return ChordResolveResult::ChordCancelled;
        }
        return ChordResolveResult::None;
    };

    // Build the full chord sequence to test.
    let test_chord: Vec<ParsedKeystroke> = match pending {
        Some(p) => {
            let mut v = p.to_vec();
            v.push(current);
            v
        }
        None => vec![current],
    };

    // Filter bindings by active contexts.
    let ctx_set: HashSet<&str> = active_contexts.iter().map(String::as_str).collect();
    let context_bindings: Vec<&ParsedBinding> = bindings
        .iter()
        .filter(|b| ctx_set.contains(b.context.as_str()))
        .collect();

    // Group prefix-extending chords by chord string so a later null-override
    // shadows the default it unbinds (resolver.ts:200-208).
    let mut chord_winners: HashMap<String, Option<String>> = HashMap::new();
    for binding in &context_bindings {
        if binding.chord.len() > test_chord.len() && chord_prefix_matches(&test_chord, binding) {
            chord_winners.insert(chord_to_string(&binding.chord), binding.action.clone());
        }
    }
    let has_longer_chords = chord_winners.values().any(Option::is_some);

    // A keystroke that could start a longer chord prefers that (even over an
    // exact single-key match).
    if has_longer_chords {
        return ChordResolveResult::ChordStarted(test_chord);
    }

    // Check for exact matches (last one wins).
    let mut exact_match: Option<&ParsedBinding> = None;
    for binding in &context_bindings {
        if chord_exactly_matches(&test_chord, binding) {
            exact_match = Some(binding);
        }
    }

    if let Some(m) = exact_match {
        return match &m.action {
            None => ChordResolveResult::Unbound,
            Some(action) => ChordResolveResult::Match(action.clone()),
        };
    }

    // No match and no potential longer chords.
    if pending.is_some() {
        return ChordResolveResult::ChordCancelled;
    }
    ChordResolveResult::None
}

/// Single-keystroke resolution (no chord state).
/// 1:1 with `resolveKey` (resolver.ts:32-61): only single-keystroke bindings,
/// last-wins, `null` → unbound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveResult {
    /// A binding matched; its action id.
    Match(String),
    /// No binding matched.
    None,
    /// A binding matched but its action is `null`.
    Unbound,
}

/// 1:1 with `resolveKey` (resolver.ts:32-61).
#[must_use]
pub fn resolve_key(
    input: &InputKey,
    active_contexts: &[String],
    bindings: &[ParsedBinding],
) -> ResolveResult {
    let ctx_set: HashSet<&str> = active_contexts.iter().map(String::as_str).collect();
    let mut found: Option<&ParsedBinding> = None;
    for binding in bindings {
        if binding.chord.len() != 1 {
            continue;
        }
        if !ctx_set.contains(binding.context.as_str()) {
            continue;
        }
        if let Some(ks) = binding.chord.first() {
            if matches_keystroke(input, ks) {
                found = Some(binding);
            }
        }
    }
    match found {
        None => ResolveResult::None,
        Some(b) => match &b.action {
            None => ResolveResult::Unbound,
            Some(a) => ResolveResult::Match(a.clone()),
        },
    }
}

/// Display text for an action in a context (e.g. `"ctrl+t"` for
/// `"app:toggleTodos"`). Searches in reverse so user overrides win.
/// 1:1 with `getBindingDisplayText` (resolver.ts:67-77).
#[must_use]
pub fn get_binding_display_text(
    action: &str,
    context: &str,
    bindings: &[ParsedBinding],
) -> Option<String> {
    bindings
        .iter()
        .rev()
        .find(|b| b.action.as_deref() == Some(action) && b.context == context)
        .map(|b| chord_to_string(&b.chord))
}
