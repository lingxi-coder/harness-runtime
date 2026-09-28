//! Shared builtin command scaffolding used by the optional `builtins` module
//! (and any platform command handlers registered directly in the composition
//! roots):
//!
//! - [`names`] — the locked 86-name table (`BUILTIN_COMMAND_NAMES`),
//!   the 19-name core list (`BUILTIN_CORE_NAMES`), `core_description`, the
//!   bucket-(d) `INTENTIONALLY_DISABLED_COMMANDS` audit table, and its STUB.6
//!   refinement into `CORRECT_BY_DESIGN_STUBS` (faithful) vs
//!   `HOST_BOUND_DEFERRED_GAPS` (genuine deferred gaps).
//! - [`help_render`] — the byte-locked `/help` screen renderer.
//! - [`list_render`] — the generic `/agents` `/hooks` `/mcp` list renderer.
//! - [`unimplemented`] — the per-name stub handler returning the locked
//!   "not implemented" literal.

pub mod help_render;
pub mod list_render;
pub mod names;
pub mod unimplemented;

pub use names::{
    core_description, is_palette_hidden, BUILTIN_COMMAND_NAMES, BUILTIN_CORE_NAMES,
    CORRECT_BY_DESIGN_STUBS, HIDDEN_PALETTE_COMMANDS, HOST_BOUND_DEFERRED_GAPS,
    INTENTIONALLY_DISABLED_COMMANDS, USAGE_CREDITS_BNR_GATED,
};
pub use unimplemented::UnimplementedCommandHandler;
