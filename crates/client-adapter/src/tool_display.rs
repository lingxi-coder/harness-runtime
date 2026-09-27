//! Lower `client_presentation::tool_display` derivations onto the wire DTOs.
//!
//! This is the seam where the ONE derivation becomes the four surfaces'
//! shared render model. `client-adapter` is the only crate allowed to touch
//! `serde_json::Value` (governing decision §0.4), so the `Value` → DTO
//! lowering happens here and `client-protocol` stays a pure contract.
//!
//! Supersedes the F1-11 note in [`crate::lowering`] that display derivation
//! "stays CLIENT-SIDE": *grouping/folding* still does, but the per-call
//! header, result headline, structured diff, and plan are computed here so
//! the terminal, iOS, Android, and Electron cannot drift apart.

use client_presentation::render::diff::{self, CodeSegment, LineKind, StructuredDiff};
use client_presentation::render::syntax::SyntaxClass;
use client_presentation::render::StyleColor;
use client_presentation::theme::ThemeName;
use client_presentation::tool_display as td;
use client_protocol::tool_display::{
    CodeSegmentDto, DiffLineKindDto, DiffRowDto, HeadlineKindDto, PlanTaskDto, PlanTaskStateDto,
    StructuredDiffDto, SyntaxClassDto, ToolHeaderDto, ToolIconDto, ToolResultDisplayDto,
    ToolSubLineDto, ToolVerbDto,
};

/// Widen a `usize` count for the wire. Counts here are bounded by the diff row
/// cap and by line counts, so saturation is unreachable in practice and is the
/// right degradation if it ever were not.
fn as_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Pack a terminal color as `0x00RRGGBB`. `None` for the terminal default and
/// for named (palette) colors, which have no fixed RGB.
fn pack_rgb(color: StyleColor) -> Option<u32> {
    match color {
        StyleColor::Rgb(r, g, b) => Some((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)),
        _ => None,
    }
}

fn lower_verb(verb: td::ToolVerb) -> ToolVerbDto {
    match verb {
        td::ToolVerb::Update => ToolVerbDto::Update,
        td::ToolVerb::Create => ToolVerbDto::Create,
        td::ToolVerb::Read => ToolVerbDto::Read,
        td::ToolVerb::Search => ToolVerbDto::Search,
        td::ToolVerb::Shell => ToolVerbDto::Shell,
        td::ToolVerb::Output => ToolVerbDto::Output,
        td::ToolVerb::Kill => ToolVerbDto::Kill,
        td::ToolVerb::Fetch => ToolVerbDto::Fetch,
        td::ToolVerb::Task => ToolVerbDto::Task,
        td::ToolVerb::Todo => ToolVerbDto::Todo,
        td::ToolVerb::Skill => ToolVerbDto::Skill,
        td::ToolVerb::Generic => ToolVerbDto::Generic,
    }
}

fn lower_icon(icon: td::ToolIcon) -> ToolIconDto {
    match icon {
        td::ToolIcon::Read => ToolIconDto::Read,
        td::ToolIcon::Search => ToolIconDto::Search,
        td::ToolIcon::List => ToolIconDto::List,
        td::ToolIcon::Edit => ToolIconDto::Edit,
        td::ToolIcon::Terminal => ToolIconDto::Terminal,
        td::ToolIcon::Globe => ToolIconDto::Globe,
        td::ToolIcon::Workflow => ToolIconDto::Workflow,
        td::ToolIcon::ListChecks => ToolIconDto::ListChecks,
        td::ToolIcon::Sparkles => ToolIconDto::Sparkles,
        td::ToolIcon::Plug => ToolIconDto::Plug,
        td::ToolIcon::Output => ToolIconDto::Output,
        td::ToolIcon::Stop => ToolIconDto::Stop,
        td::ToolIcon::Wrench => ToolIconDto::Wrench,
    }
}

fn lower_class(class: SyntaxClass) -> SyntaxClassDto {
    match class {
        SyntaxClass::Plain => SyntaxClassDto::Plain,
        SyntaxClass::Keyword => SyntaxClassDto::Keyword,
        SyntaxClass::TypeName => SyntaxClassDto::TypeName,
        SyntaxClass::Function => SyntaxClassDto::Function,
        SyntaxClass::StringLit => SyntaxClassDto::StringLit,
        SyntaxClass::Number => SyntaxClassDto::Number,
        SyntaxClass::Comment => SyntaxClassDto::Comment,
        SyntaxClass::Punctuation => SyntaxClassDto::Punctuation,
        SyntaxClass::Operator => SyntaxClassDto::Operator,
        SyntaxClass::Variable => SyntaxClassDto::Variable,
        SyntaxClass::Constant => SyntaxClassDto::Constant,
        SyntaxClass::Attribute => SyntaxClassDto::Attribute,
    }
}

fn lower_headline_kind(kind: td::result::HeadlineKind) -> HeadlineKindDto {
    use td::result::HeadlineKind as K;
    match kind {
        K::Added => HeadlineKindDto::Added,
        K::Removed => HeadlineKindDto::Removed,
        K::AddedRemoved => HeadlineKindDto::AddedRemoved,
        K::LinesRead => HeadlineKindDto::LinesRead,
        K::LinesReadPartial => HeadlineKindDto::LinesReadPartial,
        K::FilesFound => HeadlineKindDto::FilesFound,
        K::FilesFoundTruncated => HeadlineKindDto::FilesFoundTruncated,
        K::LinesFound => HeadlineKindDto::LinesFound,
        K::MatchesFound => HeadlineKindDto::MatchesFound,
        K::Interrupted => HeadlineKindDto::Interrupted,
        K::NoContent => HeadlineKindDto::NoContent,
        K::Failed => HeadlineKindDto::Failed,
        K::Plain => HeadlineKindDto::Plain,
    }
}

fn lower_kind(kind: LineKind) -> DiffLineKindDto {
    match kind {
        LineKind::Add => DiffLineKindDto::Add,
        LineKind::Remove => DiffLineKindDto::Remove,
        LineKind::Context => DiffLineKindDto::Context,
    }
}

fn lower_segment(segment: &CodeSegment) -> CodeSegmentDto {
    CodeSegmentDto {
        text: segment.text.clone(),
        class: lower_class(segment.class),
        rgb: pack_rgb(segment.fg),
        bold: segment.bold,
        italic: segment.italic,
        underline: segment.underline,
        emph: segment.emph,
    }
}

/// Lower a derived tool-call header.
#[must_use]
pub fn lower_tool_header(tool: &str, input: &serde_json::Value) -> ToolHeaderDto {
    let header = td::tool_header(tool, input);
    ToolHeaderDto {
        verb: lower_verb(header.verb),
        icon: Some(lower_icon(header.icon)),
        title: header.title(),
        label: header.label,
        primary: header.primary,
        qualifier: header.qualifier,
        count: header.count,
        sub_line: header.sub_line.map(|sub| ToolSubLineDto {
            prefix: sub.prefix,
            text: sub.text,
        }),
    }
}

/// Lower a derived structured diff.
#[must_use]
pub fn lower_structured_diff(
    structured: &StructuredDiff,
    file_path: Option<&str>,
) -> StructuredDiffDto {
    StructuredDiffDto {
        file_path: file_path.map(str::to_string),
        language: structured.language.clone(),
        gutter_width: as_u32(structured.gutter_width),
        additions: as_u32(structured.additions),
        removals: as_u32(structured.removals),
        truncated_rows: as_u32(structured.truncated_rows),
        rows: structured
            .rows
            .iter()
            .map(|row| DiffRowDto {
                kind: lower_kind(row.kind),
                line_no: as_u32(row.line_no),
                hunk: as_u32(row.hunk),
                word_diffed: row.word_diffed,
                segments: row.segments.iter().map(lower_segment).collect(),
            })
            .collect(),
    }
}

/// Lower a derived plan.
#[must_use]
pub fn lower_plan_tasks(tasks: &[td::PlanTask]) -> Vec<PlanTaskDto> {
    tasks
        .iter()
        .map(|task| PlanTaskDto {
            id: task.id.clone(),
            subject: task.subject.clone(),
            active_form: task.active_form.clone(),
            state: match task.state {
                td::PlanTaskState::Pending => PlanTaskStateDto::Pending,
                td::PlanTaskState::InProgress => PlanTaskStateDto::InProgress,
                td::PlanTaskState::Completed => PlanTaskStateDto::Completed,
            },
        })
        .collect()
}

/// The plan carried by a `TodoWrite` call's input, if this is one.
#[must_use]
pub fn plan_from_tool_call(tool: &str, input: &serde_json::Value) -> Option<Vec<PlanTaskDto>> {
    (tool == "TodoWrite")
        .then(|| lower_plan_tasks(&td::plan::plan_tasks_from_todowrite_input(input)))
}

/// Derive the complete `⎿` display block for one completed tool call.
///
/// The ONE function both the live event and the resumed scrollback call, so
/// identical content produces an identical DTO on both paths.
#[must_use]
pub fn lower_tool_result_display(
    tool: &str,
    input: Option<&serde_json::Value>,
    result: &serde_json::Value,
    is_error: bool,
) -> ToolResultDisplayDto {
    let headline = td::result::result_headline_parts(tool, input, result, is_error);

    // The diff comes from the call input, which is what the terminal renders
    // too, so the headline and the diff always describe the same comparison.
    let diff = input.and_then(|input| {
        let (old, new, file_path) = client_presentation::tool_display::diff_inputs_for(tool, input);
        if old.is_none() && new.is_none() {
            return None;
        }
        let old = old.unwrap_or_default();
        let new = new.unwrap_or_default();
        let structured = diff::structured_diff(
            &old,
            &new,
            file_path.as_deref(),
            ThemeName::default(),
            diff::MAX_WIRE_DIFF_ROWS,
        );
        (!structured.rows.is_empty())
            .then(|| lower_structured_diff(&structured, file_path.as_deref()))
    });

    let clamped = td::result::result_body(tool, result).map(|body| td::result::clamp_body(&body));
    // The `collapsed` verdict is decided ONCE, here, and covers BOTH things a
    // client mounts for a result: the body AND the structured diff. Deriving
    // it from the body alone shipped `collapsed: false` for a 400-row
    // (`MAX_WIRE_DIFF_ROWS`) diff whose body was one line, so iOS and Android
    // — both of which gate `isCollapsible` on this flag — mounted every row
    // inline with no toggle, and Electron had to invent its own row budget.
    let diff_rows = diff.as_ref().map_or(0, |d| d.rows.len());
    let collapsed = clamped.as_ref().is_some_and(|c| c.collapsed)
        || diff_rows > td::result::INLINE_DIFF_ROW_BUDGET;
    ToolResultDisplayDto {
        headline_kind: headline.as_ref().map(|h| lower_headline_kind(h.kind)),
        headline_args: headline
            .as_ref()
            .map(|h| h.counts.clone())
            .unwrap_or_default(),
        headline: headline.map(|h| h.english),
        diff,
        body: clamped.as_ref().map(|c| c.text.clone()),
        body_lines: clamped.as_ref().map_or(0, |c| as_u32(c.total_lines)),
        body_truncated: clamped.as_ref().is_some_and(|c| c.truncated),
        collapsed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn header_lowers_verb_key_and_composed_title() {
        let header = lower_tool_header("Edit", &json!({"file_path": "src/host.rs"}));
        assert_eq!(header.verb, ToolVerbDto::Update);
        assert_eq!(header.label, "Update");
        assert_eq!(header.primary.as_deref(), Some("src/host.rs"));
        assert_eq!(header.title, "Update(src/host.rs)");
        assert!(header.sub_line.is_none());
        assert_eq!(header.icon, Some(ToolIconDto::Edit));
    }

    #[test]
    fn bash_header_carries_the_command_sub_line() {
        let header = lower_tool_header("Bash", &json!({"command": "cargo test"}));
        assert_eq!(header.verb, ToolVerbDto::Shell);
        assert_eq!(header.count, Some(1));
        let sub = header.sub_line.expect("a sub-line");
        assert_eq!(sub.prefix, "$");
        assert_eq!(sub.text, "cargo test");
        assert_eq!(header.icon, Some(ToolIconDto::Terminal));
    }

    #[test]
    fn edit_result_carries_headline_and_a_structured_diff() {
        let input = json!({
            "file_path": "/tmp/x.rs",
            "old_string": "fn a() {}\n",
            "new_string": "fn b() {}\n",
        });
        let display =
            lower_tool_result_display("Edit", Some(&input), &json!({"content": "ok"}), false);
        assert_eq!(
            display.headline.as_deref(),
            Some("Added 1 line, removed 1 line")
        );
        let diff = display.diff.expect("a diff");
        assert_eq!(diff.file_path.as_deref(), Some("/tmp/x.rs"));
        assert_eq!((diff.additions, diff.removals), (1, 1));
        assert_eq!(diff.rows.len(), 2);
        // Segments reassemble the row text exactly — the contract that lets
        // clients render without indexing a string.
        let joined: String = diff.rows[1]
            .segments
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(joined, "fn b() {}");
    }

    #[test]
    fn a_result_with_no_paired_input_still_gets_a_display_without_a_diff() {
        // The orphan case: a result whose originating call was never seen.
        let display = lower_tool_result_display(
            "Read",
            None,
            &json!({"file": {"numLines": 3, "totalLines": 3, "content": "a\nb\nc"}}),
            false,
        );
        assert!(display.diff.is_none());
        assert_eq!(display.headline.as_deref(), Some("Read 3 lines"));
        assert_eq!(display.body.as_deref(), Some("a\nb\nc"));
        assert_eq!(display.body_lines, 3);
        assert!(!display.collapsed);
    }

    #[test]
    fn a_long_body_is_marked_collapsed_and_reports_its_full_line_count() {
        let content: String = (0..50).map(|n| format!("line{n}\n")).collect();
        let display = lower_tool_result_display(
            "Bash",
            None,
            &json!({"stdout": content, "stderr": "", "interrupted": false}),
            false,
        );
        assert!(display.collapsed);
        assert!(!display.body_truncated, "50 lines is under the wire cap");
        assert_eq!(display.body_lines, 50);
    }

    #[test]
    fn a_large_diff_ships_collapsed_even_when_the_body_is_short() {
        // 200 replaced lines → 400 diff rows, the `MAX_WIRE_DIFF_ROWS` cap,
        // beside a body of NONE. Deriving `collapsed` from the body alone
        // shipped `false`, and iOS/Android gate `isCollapsible` on this flag —
        // so every row mounted inline with no toggle.
        let old = "was here\n".repeat(200);
        let new = "now here\n".repeat(200);
        let input = json!({"file_path": "/tmp/x.rs", "old_string": old, "new_string": new});
        let display = lower_tool_result_display("Edit", Some(&input), &json!({}), false);
        let rows = display.diff.as_ref().expect("a diff").rows.len();
        assert!(
            rows > td::result::INLINE_DIFF_ROW_BUDGET,
            "expected a diff past the inline budget, got {rows} rows"
        );
        assert_eq!(display.body_lines, 0, "this case has no body at all");
        assert!(display.collapsed, "a {rows}-row diff must ship collapsed");
    }

    #[test]
    fn an_edit_result_never_ships_the_pre_edit_file_as_body() {
        // The literal `data` shape from `tools/file/src/edit.rs`. None of its
        // keys are body probes, so the whole object — `originalFile` and all —
        // used to become the user-visible body on every single edit.
        let original = "a line of the file being edited\n".repeat(300);
        let result = json!({
            "filePath": "/tmp/x.rs",
            "oldString": "fn a() {}\n",
            "newString": "fn b() {}\n",
            "originalFile": original,
            "structuredPatch": "-fn a() {}\n+fn b() {}\n",
            "userModified": false,
            "replaceAll": false,
        });
        let input = json!({
            "file_path": "/tmp/x.rs",
            "old_string": "fn a() {}\n",
            "new_string": "fn b() {}\n",
        });
        let display = lower_tool_result_display("Edit", Some(&input), &result, false);
        assert_eq!(display.body, None, "the diff IS the body for an edit");
        assert_eq!(display.body_lines, 0);
        assert!(display.diff.is_some(), "the diff still ships");
    }

    #[test]
    fn only_todowrite_yields_a_plan() {
        let plan = plan_from_tool_call(
            "TodoWrite",
            &json!({"todos": [{"content": "Ship", "status": "in_progress"}]}),
        )
        .expect("a plan");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].subject, "Ship");
        assert_eq!(plan[0].state, PlanTaskStateDto::InProgress);
        assert!(plan_from_tool_call("Edit", &json!({})).is_none());
    }

    #[test]
    fn syntax_colors_pack_as_rgb_and_classes_survive_lowering() {
        let input = json!({
            "file_path": "/tmp/x.rs",
            "old_string": "",
            "new_string": "fn main() {}\n",
        });
        let display = lower_tool_result_display("Edit", Some(&input), &json!({}), false);
        let diff = display.diff.expect("a diff");
        let segments = &diff.rows[0].segments;
        assert!(
            segments.iter().any(|s| s.class == SyntaxClassDto::Keyword),
            "`fn` should classify as a keyword: {segments:?}"
        );
        assert!(
            segments.iter().any(|s| s.rgb.is_some()),
            "terminal RGB is packed alongside the class"
        );
    }
}
