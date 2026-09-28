//! Slash-command runtime abstraction (M8-P9).
//!
//! The platform-agnostic command machinery: the [`CommandRegistry`],
//! [`RegistrySlashDispatcher`], parser, argument substitution, data model, and
//! the shared builtin scaffolding ([`builtin_support`] — the locked 99-name
//! table, the `/help` + list renderers, and the per-name unimplemented stub
//! handler).
//!
//! Cross-platform command handlers are available in `builtins` when the
//! `builtin-handlers` feature is enabled. Desktop/mobile composition roots
//! register their platform-specific handlers separately.
//!
//! See spec §19 for the broader slash-command design.

#![forbid(unsafe_code)]

#[cfg(feature = "builtin-handlers")]
pub mod builtins;

pub mod argument_substitution;
pub mod builtin_support;
pub mod cd;
pub mod describe;
pub mod dispatcher;
pub mod expand;
pub mod markdown_loader;
pub mod mcp_prompts;
pub mod model;
pub mod parser;
pub mod registry;
pub mod shell_expansion;
pub mod skill_usage;

pub use argument_substitution::{
    generate_progressive_argument_hint, parse_argument_names, parse_arguments,
    substitute_arguments, substitute_arguments_faithful, FrontmatterArgs, SubstitutionError,
};
pub use describe::format_description_with_source;
pub use dispatcher::{
    BackgroundPromptLauncher, McpPromptResolver, RegistrySlashDispatcher, SkillInvocationObserver,
};
pub use expand::{expand_markdown_command, ExpandCtx, ExpandError};
pub use markdown_loader::{
    build_markdown_command, build_skill_command, command_name_from_path,
    extract_description_from_markdown, load_command_markdown_files, load_skill_markdown_files,
    load_skill_markdown_files_with_roots, parse_command_markdown, parse_skill_command_markdown,
    project_dirs_up_to_home, MarkdownCommandFile, SkillMarkdownCommandFile,
};
pub use model::*;
pub use parser::{parse_slash_command, ParsedSlashCommand};
pub use registry::CommandRegistry;
pub use shell_expansion::{
    execute_shell_commands_in_prompt, ShellExpansionCtx, ShellExpansionError,
    ShellExpansionProvider, ShellOut, ShellPermissionDecision, ShellPermissionGate, ShellRunError,
    ShellRunner,
};
