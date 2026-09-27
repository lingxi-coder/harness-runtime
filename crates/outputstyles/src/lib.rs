//! Output-style subsystem: registry, model, and built-in markdown default.
//!
//! See spec §21.

#![forbid(unsafe_code)]

pub mod builtin;
pub mod disk;
pub mod model;
pub mod registry;

pub use disk::{
    load_output_styles_from_dir, parse_output_style, resolve_output_style,
    resolve_output_style_from, DiskOutputStyle, ResolvedOutputStyle,
};
pub use model::*;
pub use registry::{
    resolve_builtin_output_style, BuiltinOutputStyle, OutputStyleError, OutputStyleRegistry,
    BUILTIN_OUTPUT_STYLES, DEFAULT_OUTPUT_STYLE_NAME,
};
