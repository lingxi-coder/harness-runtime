//! Core keybinding types — a 1:1 port of `claude-code/src/keybindings/types.ts`.
//!
//! The reference `types.ts` is absent from the extracted source tree (a dangling
//! `import './types.js'`), but every shape is fully determined by the modules
//! that consume it (`parser.ts`, `resolver.ts`, `loadUserBindings.ts`):
//!
//! - `ParsedKeystroke = { key: string, ctrl/alt/shift/meta/super: boolean }`
//! - `Chord = ParsedKeystroke[]`
//! - `ParsedBinding = { chord: Chord, action: string | null, context: string }`
//! - `KeybindingBlock = { context: string, bindings: Record<string, action|null> }`
//! - `KeybindingsSchemaType = { $schema?: string, $docs?: string, bindings: KeybindingBlock[] }`
//!
//! `action` is `Option<String>` (a `null` value unbinds a default — see
//! `defaultBindings.ts` and `resolver.ts`'s null-shadows-prefix logic).

use indexmap::IndexMap;

/// A single parsed keystroke: a base key plus the five modifier flags.
///
/// 1:1 with the TS `ParsedKeystroke` produced by `parseKeystroke`. `alt` and
/// `meta` are kept as DISTINCT booleans (the config can name either) but are
/// collapsed at match/equality time — terminals can't distinguish them (see
/// `matcher::modifiers_match` / `resolver::keystrokes_equal`). `super` (cmd/win)
/// is distinct and only arrives via the kitty keyboard protocol.
// The five modifier booleans mirror the TS `ParsedKeystroke` shape 1:1
// (ctrl/alt/shift/meta/super). Collapsing them into a bitflags type would
// diverge from the byte-faithful port for no behavioral gain.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedKeystroke {
    /// The normalized base key name (e.g. `"k"`, `"escape"`, `"up"`, `" "`).
    pub key: String,
    /// `ctrl`/`control`.
    pub ctrl: bool,
    /// `alt`/`opt`/`option`.
    pub alt: bool,
    /// `shift`.
    pub shift: bool,
    /// `meta`.
    pub meta: bool,
    /// `cmd`/`command`/`super`/`win`.
    pub super_: bool,
}

/// A chord: a sequence of keystrokes (e.g. `ctrl+x ctrl+k` → two keystrokes).
/// 1:1 with the TS `Chord = ParsedKeystroke[]`.
pub type Chord = Vec<ParsedKeystroke>;

/// A flattened binding: one chord → one action (or `None` to unbind) in a
/// context. 1:1 with the TS `ParsedBinding`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBinding {
    /// The keystroke sequence.
    pub chord: Chord,
    /// The action id (e.g. `"app:toggleTodos"`, `"command:help"`), or `None`
    /// when the JSON value is `null` (an explicit unbind of a default).
    pub action: Option<String>,
    /// The UI context this binding applies in (e.g. `"Global"`, `"Chat"`).
    pub context: String,
}

/// One block from `keybindings.json`: a context plus its `key → action` map.
/// 1:1 with the TS `KeybindingBlock`. The map is insertion-ordered
/// ([`IndexMap`]) so the default→user merge and last-wins resolution preserve
/// the same iteration order as JS `Object.entries`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingBlock {
    /// The context name (validated against [`super::schema::KEYBINDING_CONTEXTS`]).
    pub context: String,
    /// Keystroke pattern → action (`None` = `null` unbind), in insertion order.
    pub bindings: IndexMap<String, Option<String>>,
}
