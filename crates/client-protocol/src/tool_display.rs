//! Tool-display DTOs — the pre-derived render model for one tool call.
//!
//! Derived ONCE in Rust (`tui_core::tool_display` + `tui_core::render::diff`)
//! and lowered onto the wire by `client-adapter`. Clients apply styling only;
//! they must not re-parse `input_json`/`result_json` to rebuild a header.
//!
//! Three shape decisions are load-bearing and should not be "simplified":
//!
//! 1. **Segments, never string offsets.** Rust indexes strings by UTF-8 byte,
//!    Swift by grapheme cluster, Kotlin and JS by UTF-16 code unit. One
//!    `(start, end)` pair therefore means four different substrings on four
//!    surfaces, and this repo's own sources are full of CJK. Every diff row
//!    ships pre-split runs whose `text` concatenates back to the row.
//! 2. **A semantic syntax class, plus the resolved terminal RGB.** The RGB is
//!    baked against one dark `.tmTheme`; a client with a light mode or a
//!    runtime theme toggle must use [`SyntaxClassDto`] and its own palette.
//! 3. **A stable verb key alongside the English label.** Mobile ships five
//!    languages; a rendered `"Update"` on the wire would be a hard
//!    localization regression. The terminal and the Electron desktop (neither
//!    of which localizes) use the label.
//!
//! Diff BACKGROUNDS are deliberately absent: the terminal's are
//! `alpha_over_black` blends valid only over a black terminal. Clients derive
//! theirs from [`DiffLineKindDto`].

// Field semantics are defined by the enclosing DTO and its frozen snapshot.
// Omitting per-field docs also keeps UniFFI's fixed per-item metadata buffer
// (`uniffi_core::metadata::BUF_SIZE == 16384`) clear of its cap.
#![allow(missing_docs)]

use serde::{Deserialize, Serialize};

/// A stable, non-localized verb identity for a tool-call header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolVerbDto {
    Update,
    Create,
    Read,
    Search,
    Shell,
    Output,
    Kill,
    Fetch,
    Task,
    Todo,
    Skill,
    Generic,
}

/// Stable semantic icon identity for one tool-call header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolIconDto {
    Read,
    Search,
    List,
    Edit,
    Terminal,
    Globe,
    Workflow,
    ListChecks,
    Sparkles,
    Plug,
    Output,
    Stop,
    Wrench,
}

/// A second header line with its own glyph, e.g. `$ cargo test --all`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ToolSubLineDto {
    pub prefix: String,
    pub text: String,
}

/// The parameterized tool-call header — `Update(src/host.rs)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ToolHeaderDto {
    pub verb: ToolVerbDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<ToolIconDto>,
    /// English label. Localizing clients key off `verb` instead.
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_line: Option<ToolSubLineDto>,
    /// Pre-composed `label(primary)qualifier`, for surfaces that do not compose.
    pub title: String,
}

/// Theme-independent semantic class of one code run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SyntaxClassDto {
    Plain,
    Keyword,
    TypeName,
    Function,
    StringLit,
    Number,
    Comment,
    Punctuation,
    Operator,
    Variable,
    Constant,
    Attribute,
}

/// Add / remove / context classification of a diff row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiffLineKindDto {
    Add,
    Remove,
    Context,
}

/// One pre-split run of a diff row's text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct CodeSegmentDto {
    pub text: String,
    pub class: SyntaxClassDto,
    /// Terminal-resolved foreground, packed `0x00RRGGBB`. Absent for the
    /// terminal default. Prefer `class`; this is baked against a dark theme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rgb: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub underline: bool,
    /// A changed word of a word-diffed pair — render with the stronger
    /// intra-line emphasis background.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub emph: bool,
}

/// One diff row: gutter metadata plus its content runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DiffRowDto {
    pub kind: DiffLineKindDto,
    /// New-file line number for add/context; old-file for remove.
    pub line_no: u32,
    /// 0-based hunk index; a change between consecutive rows is where the
    /// `⋯` separator belongs.
    pub hunk: u32,
    /// The row was word-diffed, so it carries `emph` runs and no syntax class.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub word_diffed: bool,
    pub segments: Vec<CodeSegmentDto>,
}

/// A complete structured diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct StructuredDiffDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Width of the right-aligned line-number gutter, across ALL hunks.
    pub gutter_width: u32,
    pub additions: u32,
    pub removals: u32,
    /// Rows dropped by the wire cap; `0` when complete.
    pub truncated_rows: u32,
    pub rows: Vec<DiffRowDto>,
}

/// What a result headline says, as a stable non-localized key.
///
/// Localizing clients look their copy up by this and substitute
/// [`ToolResultDisplayDto::headline_args`]; the terminal and the Electron
/// desktop render [`ToolResultDisplayDto::headline`] directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HeadlineKindDto {
    /// `args = [additions]`.
    Added,
    /// `args = [removals]`.
    Removed,
    /// `args = [additions, removals]`.
    AddedRemoved,
    /// `args = [read]`.
    LinesRead,
    /// `args = [read, total]`.
    LinesReadPartial,
    /// `args = [n]`.
    FilesFound,
    /// `args = [n]`.
    FilesFoundTruncated,
    /// `args = [n]`.
    LinesFound,
    /// `args = [n]`.
    MatchesFound,
    Interrupted,
    NoContent,
    /// The message is in `headline`.
    Failed,
    /// Free text; the message is in `headline`.
    Plain,
}

/// Everything a client needs to render one completed call's `⎿` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ToolResultDisplayDto {
    /// `Added 18 lines`, `Found 3 files`, … in ENGLISH. Absent when there is
    /// nothing to say (TodoWrite, whose checklist renders instead).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headline: Option<String>,
    /// What the headline says, for clients that localize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headline_kind: Option<HeadlineKindDto>,
    /// Numeric slots for `headline_kind`, in the order it documents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headline_args: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<StructuredDiffDto>,
    /// Plain-text body for the expanded view, clamped to the wire caps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Line count BEFORE clamping — drives a client's "show N more lines".
    pub body_lines: u32,
    /// `body` was clamped; the full text remains in `result_json`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub body_truncated: bool,
    /// The body exceeds the inline budget — render it collapsed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub collapsed: bool,
}

/// Lifecycle state of one plan task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlanTaskStateDto {
    Pending,
    InProgress,
    Completed,
}

/// One item of the model-managed working plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PlanTaskDto {
    /// Stable V2 task id. TodoWrite V1 items have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub subject: String,
    /// Present-continuous label, for the status line — not the list row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    pub state: PlanTaskStateDto,
}
