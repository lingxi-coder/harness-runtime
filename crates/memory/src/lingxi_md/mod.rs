//! LINGXI.md hierarchy loader.
//!
//! Discovers the full claude-code memory-file set (`getMemoryFiles`): the
//! Managed tier (`<managed>/LINGXI.md` + `.lingxi/rules/**`), the User tier
//! (`~/.lingxi/LINGXI.md` + `~/.lingxi/rules/**`), and, per directory from the
//! filesystem root down to cwd, `LINGXI.md`, `.lingxi/LINGXI.md`,
//! `.lingxi/rules/**`, and `LINGXI.local.md`. Each file is read whole (no size
//! cap — parity with claude-code `readFile`). See [`hierarchy::walk`].

pub mod agents;
pub mod excludes;
pub mod hierarchy;
pub mod loader;

pub use excludes::LingxiMdExcluder;
pub use hierarchy::{user_config_dir, Hierarchy, HierarchyEntry};
pub use loader::{LoadedFile, LoaderError};

/// Which LINGXI.md tier a discovered file belongs to.
///
/// 1:1 with claude-code `MemoryType` (`utils/memory/types.ts`) restricted to
/// the four instruction tiers this port loads — the separate memdir tiers
/// (`AutoMem` / `TeamMem`) are out of scope, and are spelled
/// [`lingxi_core::types::MemoryEntryTier`].
///
/// The splice / discovery order the tiers drive is NOT a property of this type
/// — see [`hierarchy::walk`], which encodes it. The injection description
/// (`getLingxiMds`, claudemd.ts:1168-1186) is likewise a function, in
/// [`loader`]: Managed and User share the "private global instructions"
/// wording; Project and Local each have their own.
pub use lingxi_core::types::SettingsScope as LingxiMdTier;
