//! The one scope vocabulary.
//!
//! Before this module the engine modelled "which rung does this configuration
//! sit on" twenty-four separate times — `SettingsLayer`, `HookSource`,
//! `SkillSource`, `CommandSource`, `ConfigScope`, `MemoryTier`, … — across
//! twenty-one crates. They drifted, and the drift has already cost a bug:
//! `permission`'s citation ladder once ranked `session` highest, the reverse of
//! the reference implementation.
//!
//! Three facts were tangled together in those enums. This module separates
//! them:
//!
//! 1. [`Scope`] — **which store** the thing lives in (`~/.lingxi/`, the project
//!    tree, the binary, this session's memory, …).
//! 2. Each subsystem's own *producer* set — plugin, MCP server, agent
//!    front-matter, skill bundle — which stays in that subsystem. Those sets
//!    are all slightly different and none of them ever drifted; hoisting them
//!    into one shared enum only hands every `match` arms it can never reach.
//!    A source enum therefore looks like
//!    `enum HookSource { Settings(SettingsScope), Plugin, Skill, … }`: the rung
//!    half shared, the producer half exact.
//! 3. Resolved locations (`repo_root`, `team_dir`, …) — deliberately NOT here.
//!    Those belong to whichever crate resolved them, next to a [`Scope`] field.
//!
//! # `Scope` is unordered on purpose
//!
//! It would be natural to `derive(PartialOrd, Ord)` here and call the result
//! "the precedence ladder". That would be wrong, and the codebase proves it:
//! the same rungs are ordered several mutually contradictory ways, each
//! correct for its own question.
//!
//! | Question | `User` vs `Project` vs `Local` |
//! |---|---|
//! | Which settings *value* wins? | `Local` > `Project` > `User` |
//! | Which rule does a denial *cite*? | `User` > `Project` > `Local` |
//! | What order is `LINGXI.md` spliced in? | `User` before `Project` before `Local` |
//!
//! The first two are exact opposites and both match the reference
//! implementation — value-precedence and citation-precedence are genuinely
//! different axes. A derived `Ord` would silently pick one and invert the
//! others, which is the same failure that produced the original bug, only
//! harder to see.
//!
//! So: **`Scope` carries identity, never order.** Each question owns a named
//! ordering function next to the code that asks it, and each such function owns
//! a test pinning the pairs it depends on. The `scope_is_not_ordered` test
//! below keeps a comparison trait from being derived here.
//!
//! ## The orderings, and the question each answers
//!
//! There is no canonical order. These all rank overlapping rungs, and several
//! disagree with each other on purpose. Before adding a twelfth, check whether
//! one of these already answers your question — and before "fixing" an apparent
//! inconsistency between two of them, read both questions.
//!
//! | Question | Where |
//! |---|---|
//! | Which settings VALUE wins a conflict? | `core::settings::Settings::load_with_layers` |
//! | Which rule does a denial CITE? | `permission::PermissionRuleSource::priority` |
//! | What order is `LINGXI.md` spliced into the prompt? | `memory::lingxi_md::hierarchy::walk` |
//! | Which tier's command/skill definition wins a name clash? | `command_api::markdown_loader` |
//! | May a later load replace a registered command? | `command_api::registry::CommandRegistry::register_command` |
//! | Which status-line config is used? | `cli::mode::read_status_line_configs_from` |
//! | Which agent directory wins? | `agent::catalog::agent_dir_precedence` |
//! | Which memory is most RELEVANT? | `memory::memdir::find::tier_weight_bps` |
//! | Which memory tier gets scarce BUDGET first? | `memory::memdir::scan::tier_priority` |
//! | Which settings files does the admin panel read, in order? | `configuration_admin::settings_bridge::FILE_LAYERS` |
//! | Which scope does `plugin enable` auto-detect first? | `configuration_admin::plugin_settings::SCOPES` |
//!
//! Two pairs are known to disagree, each for a good reason, and each is pinned
//! by a test that says so: citation precedence runs `user > project > local`
//! while value precedence runs the other way
//! (`citation_precedence_is_the_reverse_of_value_precedence_for_the_file_tiers`),
//! and memdir ranks `Project` second for relevance but last for budget
//! (`the_two_tier_orderings_disagree_on_purpose`).
//!
//! A twelfth entry is a partition rather than an order, and is security-bearing:
//! `agent::hooks_trust::source_is_self_trusting` decides which agent sources
//! must prove folder trust.

use serde::{Deserialize, Serialize};

/// Which store a piece of configuration lives in.
///
/// Identity only — see the module docs for why this type is deliberately not
/// ordered and not comparable beyond equality.
///
/// The derived `Serialize` form is PascalCase (`"Project"`), which preserves
/// the historical `MemoryEntry` round-trip. It is **one** of several spellings
/// this engine emits, not the canonical one: the hook payload wants
/// `snake_case`, the MCP telemetry inventory wants a different bucket name
/// entirely (`Local` reports as `"user"` there), and the settings admin panel
/// wants its own. Never reach for the derived form to satisfy one of those —
/// each has its own mapping function, and they are not interchangeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scope {
    /// Compiled into the binary. No file backs it.
    Builtin,
    /// Environment variables.
    Env,
    /// Arguments given on the command line for this run.
    Cli,
    /// Configuration attached to a feature flag.
    Flag,
    /// Lives only for the current session; never written to disk.
    Session,
    /// `<project>/.lingxi/*.local.json` — per-clone, gitignored.
    Local,
    /// `<project>/.lingxi/` — checked into the project.
    Project,
    /// `~/.lingxi/` — this user, all projects.
    User,
    /// A shared team directory.
    Team,
    /// Administrator-managed policy.
    Managed,
}

/// The three rungs a user can WRITE configuration to.
///
/// Six of the enums this module replaced were *exactly* this subset —
/// `configuration-admin`'s `Scope`, the CLI's `Scope`, `SettingsSource`,
/// `AgentMemoryScope`, and the two wire DTOs — so it earns a shared type
/// rather than a sixfold repeat.
///
/// It exists to keep a guarantee that a bare [`Scope`] would lose. A field
/// typed `Option<Scope>` accepts `Managed` and `Env`; several of those six
/// reach `serde` on a production path (an `--agents` JSON blob is
/// deserialized straight into `AgentDefinition`), so widening the type would
/// quietly widen what those paths accept, in the permissive direction. The
/// narrowing is therefore a type, not a runtime check: values outside the
/// triple fail to parse instead of being honoured.
///
/// Converting *up* to [`Scope`] is total; converting *down* is fallible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WritableScope {
    /// `~/.lingxi/settings.json`.
    User,
    /// `<project>/.lingxi/settings.json` — checked in.
    Project,
    /// `<project>/.lingxi/settings.local.json` — gitignored.
    Local,
}

impl From<WritableScope> for Scope {
    fn from(value: WritableScope) -> Self {
        match value {
            WritableScope::User => Self::User,
            WritableScope::Project => Self::Project,
            WritableScope::Local => Self::Local,
        }
    }
}

impl TryFrom<Scope> for WritableScope {
    type Error = Scope;

    /// Returns the offending [`Scope`] as the error so a caller can name it in
    /// a message ("cannot write to the managed settings tier").
    fn try_from(value: Scope) -> Result<Self, Self::Error> {
        match value {
            Scope::User => Ok(Self::User),
            Scope::Project => Ok(Self::Project),
            Scope::Local => Ok(Self::Local),
            other => Err(other),
        }
    }
}

/// Every rung backed by a settings file, including the read-only
/// administrator-managed one.
///
/// Distinct from [`WritableScope`] by exactly one variant, and the difference
/// is load-bearing: `Managed` is a tier the engine READS (it is spliced into
/// `LINGXI.md` and reported on the `InstructionsLoaded` hook payload) but never
/// writes, which is why `plugin enable --scope managed` is refused.
///
/// Narrower than [`Scope`] for two different reasons depending on the caller.
/// For the hook payload it is wire safety — 1:1 with claude-code's
/// `INSTRUCTIONS_MEMORY_TYPES`, so a bare [`Scope`] would let the engine emit
/// `Team` or `Session` tiers the reference never sends. For the `LINGXI.md`
/// splice it is just honesty: that code can never see the other six rungs, and
/// a wide type would hand every `match` six unreachable arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SettingsScope {
    /// `~/.lingxi/settings.json`.
    User,
    /// `<project>/.lingxi/settings.json` — checked in.
    Project,
    /// `<project>/.lingxi/settings.local.json` — gitignored.
    Local,
    /// Administrator-managed policy. Read-only.
    Managed,
}

impl From<WritableScope> for SettingsScope {
    fn from(value: WritableScope) -> Self {
        match value {
            WritableScope::User => Self::User,
            WritableScope::Project => Self::Project,
            WritableScope::Local => Self::Local,
        }
    }
}

impl TryFrom<SettingsScope> for WritableScope {
    type Error = SettingsScope;

    /// Fails only on [`SettingsScope::Managed`] — the one tier that is read but
    /// never written.
    fn try_from(value: SettingsScope) -> Result<Self, Self::Error> {
        match value {
            SettingsScope::User => Ok(Self::User),
            SettingsScope::Project => Ok(Self::Project),
            SettingsScope::Local => Ok(Self::Local),
            SettingsScope::Managed => Err(value),
        }
    }
}

impl From<SettingsScope> for Scope {
    fn from(value: SettingsScope) -> Self {
        match value {
            SettingsScope::User => Self::User,
            SettingsScope::Project => Self::Project,
            SettingsScope::Local => Self::Local,
            SettingsScope::Managed => Self::Managed,
        }
    }
}

impl TryFrom<Scope> for SettingsScope {
    type Error = Scope;

    fn try_from(value: Scope) -> Result<Self, Self::Error> {
        match value {
            Scope::User => Ok(Self::User),
            Scope::Project => Ok(Self::Project),
            Scope::Local => Ok(Self::Local),
            Scope::Managed => Ok(Self::Managed),
            other => Err(other),
        }
    }
}

/// The four tiers a memdir entry can live in.
///
/// A narrowing of [`Scope`] like the two above, but it earns its own type for a
/// different reason: `memdir` ranks these four three separate ways (relevance
/// weight, tie-break order, and what survives a byte budget), and each ranking
/// is a `match`. Widening to [`Scope`] would hand all three six arms that can
/// never be reached.
///
/// `Team` appears here and nowhere else in the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MemoryEntryTier {
    /// Session-scoped entry.
    Session,
    /// Project-scoped entry.
    Project,
    /// Team-scoped entry (subject to the team-boost gate).
    Team,
    /// User-scoped entry.
    User,
}

impl From<MemoryEntryTier> for Scope {
    fn from(value: MemoryEntryTier) -> Self {
        match value {
            MemoryEntryTier::Session => Self::Session,
            MemoryEntryTier::Project => Self::Project,
            MemoryEntryTier::Team => Self::Team,
            MemoryEntryTier::User => Self::User,
        }
    }
}

impl TryFrom<Scope> for MemoryEntryTier {
    type Error = Scope;

    fn try_from(value: Scope) -> Result<Self, Self::Error> {
        match value {
            Scope::Session => Ok(Self::Session),
            Scope::Project => Ok(Self::Project),
            Scope::Team => Ok(Self::Team),
            Scope::User => Ok(Self::User),
            other => Err(other),
        }
    }
}

/// `snake_case` serde for [`Scope`], for wires that spell the rungs that way.
///
/// The type derives PascalCase, which is what `MemoryEntry` has always
/// round-tripped and what the `InstructionsLoaded` hook payload sends. Other
/// wires spell the same rungs `snake_case`. Neither is "the" spelling — that is
/// why this is an opt-in adapter on a field rather than a `rename_all` on the
/// type, which could only ever serve one of them.
///
/// Use it as `#[serde(with = "crate::types::scope::snake_case")]`.
pub mod snake_case {
    use super::Scope;
    use serde::{Deserialize, Deserializer, Serializer};

    /// The `snake_case` spelling of each rung.
    #[must_use]
    pub const fn name(scope: Scope) -> &'static str {
        match scope {
            Scope::Builtin => "builtin",
            Scope::Env => "env",
            Scope::Cli => "cli",
            Scope::Flag => "flag",
            Scope::Session => "session",
            Scope::Local => "local",
            Scope::Project => "project",
            Scope::User => "user",
            Scope::Team => "team",
            Scope::Managed => "managed",
        }
    }

    /// Parse a `snake_case` rung; `None` when unrecognized.
    #[must_use]
    pub fn parse(value: &str) -> Option<Scope> {
        Some(match value {
            "builtin" => Scope::Builtin,
            "env" => Scope::Env,
            "cli" => Scope::Cli,
            "flag" => Scope::Flag,
            "session" => Scope::Session,
            "local" => Scope::Local,
            "project" => Scope::Project,
            "user" => Scope::User,
            "team" => Scope::Team,
            "managed" => Scope::Managed,
            _ => return None,
        })
    }

    /// Serialize as the `snake_case` spelling.
    ///
    /// # Errors
    /// Never fails itself; propagates the serializer's own error.
    pub fn serialize<S: Serializer>(scope: &Scope, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(name(*scope))
    }

    /// Deserialize from the `snake_case` spelling.
    ///
    /// # Errors
    /// Returns an error for any value outside the ten rungs.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Scope, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse(&raw).ok_or_else(|| serde::de::Error::custom(format!("unknown scope: {raw}")))
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryEntryTier, Scope, SettingsScope, WritableScope};

    /// Both sides of the narrowing, because only pinning the accepted side
    /// would let the gate widen silently — and it widens in the permissive
    /// direction, which is the dangerous one.
    #[test]
    fn settings_scope_accepts_exactly_the_three_editable_rungs() {
        for (scope, expected) in [
            (Scope::User, WritableScope::User),
            (Scope::Project, WritableScope::Project),
            (Scope::Local, WritableScope::Local),
        ] {
            assert_eq!(WritableScope::try_from(scope), Ok(expected));
            assert_eq!(Scope::from(expected), scope, "round trip");
        }

        for rejected in [
            Scope::Builtin,
            Scope::Env,
            Scope::Cli,
            Scope::Flag,
            Scope::Session,
            Scope::Team,
            Scope::Managed,
        ] {
            assert_eq!(
                WritableScope::try_from(rejected),
                Err(rejected),
                "{rejected:?} is not user-editable and must not narrow"
            );
        }
    }

    /// `Managed` is the single variant separating the two narrowings, and the
    /// whole point of keeping them apart — it is a tier the engine reads but
    /// must never write. Pinned on both sides so the boundary cannot drift in
    /// either direction.
    #[test]
    fn managed_reads_as_a_settings_tier_but_never_narrows_to_a_writable_one() {
        assert_eq!(
            SettingsScope::try_from(Scope::Managed),
            Ok(SettingsScope::Managed)
        );
        assert_eq!(
            WritableScope::try_from(SettingsScope::Managed),
            Err(SettingsScope::Managed),
            "Managed is read-only; narrowing it to a writable rung would make \
             `plugin enable --scope managed` succeed"
        );

        for writable in [
            WritableScope::User,
            WritableScope::Project,
            WritableScope::Local,
        ] {
            let tier = SettingsScope::from(writable);
            assert_eq!(WritableScope::try_from(tier), Ok(writable), "round trip");
        }
    }

    /// The six rungs that are not settings files at all must not narrow to a
    /// tier either — otherwise `InstructionsLoaded` could report a memory type
    /// claude-code never sends.
    #[test]
    fn settings_scope_admits_exactly_the_four_file_tiers() {
        for rejected in [
            Scope::Builtin,
            Scope::Env,
            Scope::Cli,
            Scope::Flag,
            Scope::Session,
            Scope::Team,
        ] {
            assert_eq!(
                SettingsScope::try_from(rejected),
                Err(rejected),
                "{rejected:?}"
            );
        }
    }

    /// Pins the `snake_case` spellings and their round trip. These are a
    /// different wire's names for the same rungs as the derived PascalCase
    /// form — both are live, which is the reason neither is a method on the
    /// type.
    #[test]
    fn snake_case_names_round_trip_for_every_rung() {
        for (scope, expected) in [
            (Scope::Builtin, "builtin"),
            (Scope::Env, "env"),
            (Scope::Cli, "cli"),
            (Scope::Flag, "flag"),
            (Scope::Session, "session"),
            (Scope::Local, "local"),
            (Scope::Project, "project"),
            (Scope::User, "user"),
            (Scope::Team, "team"),
            (Scope::Managed, "managed"),
        ] {
            assert_eq!(super::snake_case::name(scope), expected);
            assert_eq!(super::snake_case::parse(expected), Some(scope));
        }
        assert_eq!(
            super::snake_case::parse("Project"),
            None,
            "not the PascalCase form"
        );
        assert_eq!(super::snake_case::parse("nope"), None);
    }

    /// The reason this is a type and not a runtime check: these values reach
    /// `serde` on a production path (`--agents` JSON → `AgentDefinition`).
    /// A field typed `Scope` would newly accept every rung above.
    #[test]
    fn settings_scope_refuses_a_wider_rung_through_serde() {
        assert!(serde_json::from_str::<WritableScope>("\"Project\"").is_ok());
        assert!(serde_json::from_str::<WritableScope>("\"Managed\"").is_err());
        assert!(serde_json::from_str::<WritableScope>("\"Session\"").is_err());

        // The wide type does accept them — which is exactly what the narrow
        // type is protecting those fields from.
        assert!(serde_json::from_str::<Scope>("\"Managed\"").is_ok());
    }

    /// `MemoryEntry.tier` has always serialized PascalCase. Pinned on both the
    /// narrow type that carries the field and the wide one it converts to, so
    /// the two cannot drift apart.
    #[test]
    fn the_memory_tier_rungs_keep_their_historical_pascal_case_spelling() {
        for (tier, expected) in [
            (MemoryEntryTier::Session, "\"Session\""),
            (MemoryEntryTier::Project, "\"Project\""),
            (MemoryEntryTier::Team, "\"Team\""),
            (MemoryEntryTier::User, "\"User\""),
        ] {
            assert_eq!(serde_json::to_string(&tier).unwrap(), expected);
            assert_eq!(
                serde_json::to_string(&Scope::from(tier)).unwrap(),
                expected,
                "the wide spelling must match the narrow one"
            );
            assert_eq!(MemoryEntryTier::try_from(Scope::from(tier)), Ok(tier));
        }

        for outside in [
            Scope::Builtin,
            Scope::Env,
            Scope::Cli,
            Scope::Flag,
            Scope::Local,
            Scope::Managed,
        ] {
            assert_eq!(
                MemoryEntryTier::try_from(outside),
                Err(outside),
                "{outside:?}"
            );
        }
    }

    /// Guards the module's central claim. `Scope` must never gain a comparison
    /// beyond equality: several subsystems order these rungs in mutually
    /// contradictory ways (settings-value precedence puts `Local` above `User`,
    /// denial citation puts `User` above `Local`), so a single derived order
    /// would silently invert one of them.
    ///
    /// Written as an autoref-specialization probe rather than a grep, so it
    /// cannot be fooled by a commented-out derive or a reformatted attribute.
    /// The *bounded inherent* method exists only while `T: PartialOrd`, and an
    /// inherent method shadows the trait one — so `probe()` answers `true`
    /// exactly when the type is ordered, and deriving an order on [`Scope`]
    /// turns this test red.
    #[test]
    fn scope_is_not_ordered() {
        struct Probe<T>(std::marker::PhantomData<T>);

        // Fallback: applies to every `Probe<T>`.
        trait Unordered {
            fn probe(&self) -> bool {
                false
            }
        }
        impl<T> Unordered for Probe<T> {}

        // Shadows the fallback, but only exists when `T` is ordered.
        impl<T: PartialOrd> Probe<T> {
            #[allow(clippy::unused_self)]
            fn probe(&self) -> bool {
                true
            }
        }

        // The probe itself must be able to say "yes" — otherwise the assertion
        // below would pass for a type that IS ordered, and prove nothing.
        assert!(
            Probe::<u8>(std::marker::PhantomData).probe(),
            "probe is broken: it cannot detect an ordered type"
        );

        assert!(
            !Probe::<Scope>(std::marker::PhantomData).probe(),
            "Scope gained a PartialOrd/Ord impl — see the module docs: the \
             rungs have several conflicting correct orders, so precedence \
             belongs in a named function per question, never on the type."
        );
    }
}
