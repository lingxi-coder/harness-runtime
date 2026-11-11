//! The bounded native output shapes for Edit, Write, and NotebookEdit.
//! Scalar checks and property stripping share the existing native Zod helpers;
//! patch and Git-diff structures are the explicit shapes in the pinned factory.

use crate::Utf16JsonProjection;
use tool_api::native_output::{
    invalid, parse_object, OutputField, OutputFieldType, UnknownOutputKeys,
};
use tool_api::tool_trait::ToolError;

const fn field(name: &'static str, kind: OutputFieldType, required: bool) -> OutputField {
    OutputField {
        name,
        kind,
        required,
    }
}
const PATCH_FIELDS: &[OutputField] = &[
    field("oldStart", OutputFieldType::Number, true),
    field("oldLines", OutputFieldType::Number, true),
    field("newStart", OutputFieldType::Number, true),
    field("newLines", OutputFieldType::Number, true),
    field("lines", OutputFieldType::StringArray, true),
];
const GIT_FIELDS: &[OutputField] = &[
    field("filename", OutputFieldType::String, true),
    field(
        "status",
        OutputFieldType::StringEnum(&["modified", "added"]),
        true,
    ),
    field("additions", OutputFieldType::Number, true),
    field("deletions", OutputFieldType::Number, true),
    field("changes", OutputFieldType::Number, true),
    field("patch", OutputFieldType::String, true),
    field("repository", OutputFieldType::StringOrNull, false),
];
const EDIT_FIELDS: &[OutputField] = &[
    field("filePath", OutputFieldType::String, true),
    field("oldString", OutputFieldType::String, true),
    field("newString", OutputFieldType::String, true),
    field("originalFile", OutputFieldType::StringOrNull, true),
    field(
        "structuredPatch",
        OutputFieldType::ObjectArray(PATCH_FIELDS),
        true,
    ),
    field("userModified", OutputFieldType::Boolean, true),
    field("replaceAll", OutputFieldType::Boolean, true),
    field("gitDiff", OutputFieldType::ObjectShape(GIT_FIELDS), false),
    field("staged", OutputFieldType::Boolean, false),
    field("stagedWording", OutputFieldType::String, false),
    // Native across-process metadata which LingXi also retains on local calls.
    field("staleRecovered", OutputFieldType::Boolean, false),
    field("contentNotInModelContext", OutputFieldType::Boolean, false),
    field("syncedSkillNext", OutputFieldType::String, false),
];
const WRITE_FIELDS: &[OutputField] = &[
    field(
        "type",
        OutputFieldType::StringEnum(&["create", "update"]),
        true,
    ),
    field("filePath", OutputFieldType::String, true),
    field("content", OutputFieldType::String, true),
    field(
        "structuredPatch",
        OutputFieldType::ObjectArray(PATCH_FIELDS),
        true,
    ),
    field("originalFile", OutputFieldType::StringOrNull, true),
    field("gitDiff", OutputFieldType::ObjectShape(GIT_FIELDS), false),
    field("userModified", OutputFieldType::Boolean, false),
    field("staged", OutputFieldType::Boolean, false),
    field("stagedWording", OutputFieldType::String, false),
    field("memdirStamped", OutputFieldType::Boolean, false),
    field("syncedSkillNext", OutputFieldType::String, false),
];
const NOTEBOOK_FIELDS: &[OutputField] = &[
    field("new_source", OutputFieldType::String, true),
    field("old_source", OutputFieldType::String, false),
    field("cell_id", OutputFieldType::String, false),
    field(
        "cell_type",
        OutputFieldType::StringEnum(&["code", "markdown"]),
        true,
    ),
    field("language", OutputFieldType::String, true),
    field("edit_mode", OutputFieldType::String, true),
    field("error", OutputFieldType::String, false),
    field("notebook_path", OutputFieldType::String, true),
    field("original_file", OutputFieldType::String, true),
    field("updated_file", OutputFieldType::String, true),
];
const STAGED_WORDING: &[&str] = &["review", "card", "cloud", "linked", "policy"];
const SYNCED_SKILL_NEXT: &[&str] = &[
    "save_tool",
    "propose_tool",
    "send_file",
    "no_save_tool",
    "report_unsaved",
];

/// Parse one of the three explicitly mapped native tool output shapes.
pub(crate) fn parse(
    tool: &str,
    output: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, ToolError> {
    let fields = match tool {
        "Edit" => EDIT_FIELDS,
        "Write" => WRITE_FIELDS,
        "NotebookEdit" => NOTEBOOK_FIELDS,
        _ => return Err(invalid(tool, "native output contract is not mapped")),
    };
    let mut prepared = None;
    // These metadata enums use optional().catch(undefined) natively. Invalid
    // metadata disappears rather than making an otherwise valid result fail.
    for (name, values) in [
        ("stagedWording", STAGED_WORDING),
        ("syncedSkillNext", SYNCED_SKILL_NEXT),
    ] {
        if output.contains_property(name)
            && !output
                .string_at(&format!("/{name}"))
                .is_some_and(|value| values.contains(&value))
        {
            prepared
                .get_or_insert_with(|| output.clone())
                .remove_field(name)
                .map_err(|_| invalid(tool, "invalid result metadata"))?;
        }
    }
    parse_object(
        prepared.as_ref().unwrap_or(output),
        tool,
        fields,
        UnknownOutputKeys::Strip,
    )
}
