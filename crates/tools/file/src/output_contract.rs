//! The bounded native output shapes for Edit, Write, and NotebookEdit.
//! Scalar checks and property stripping share the existing native Zod helpers;
//! patch and Git-diff structures are the explicit shapes in the pinned factory.

use crate::Utf16JsonProjection;
use tool_api::native_output::{
    array_values, invalid, parse_object, string_array, OutputField, OutputFieldType,
    UnknownOutputKeys,
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
    field("lines", OutputFieldType::Array, true),
];
const GIT_FIELDS: &[OutputField] = &[
    field("filename", OutputFieldType::String, true),
    field("status", OutputFieldType::String, true),
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
    field("structuredPatch", OutputFieldType::Array, true),
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
    field("type", OutputFieldType::String, true),
    field("filePath", OutputFieldType::String, true),
    field("content", OutputFieldType::String, true),
    field("structuredPatch", OutputFieldType::Array, true),
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
    field("cell_type", OutputFieldType::String, true),
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
    let mut prepared = output.clone();
    // These metadata enums use optional().catch(undefined) natively. Invalid
    // metadata disappears rather than making an otherwise valid result fail.
    for (name, values) in [
        ("stagedWording", STAGED_WORDING),
        ("syncedSkillNext", SYNCED_SKILL_NEXT),
    ] {
        if prepared.contains_property(name)
            && !prepared
                .string_at(&format!("/{name}"))
                .is_some_and(|value| values.contains(&value))
        {
            prepared
                .remove_field(name)
                .map_err(|_| invalid(tool, "invalid result metadata"))?;
        }
    }
    let mut parsed = parse_object(&prepared, tool, fields, UnknownOutputKeys::Strip)?;
    if tool == "NotebookEdit" {
        require_enum(&parsed, "/cell_type", &["code", "markdown"], tool)?;
        return Ok(parsed);
    }
    if tool == "Write" {
        require_enum(&parsed, "/type", &["create", "update"], tool)?;
    }
    let mut patches = Vec::new();
    for patch in array_values(&parsed, "/structuredPatch", tool, "structuredPatch")? {
        let patch = parse_object(&patch, tool, PATCH_FIELDS, UnknownOutputKeys::Strip)?;
        string_array(&patch, "/lines", tool, "lines")?;
        patches.push(patch);
    }
    parsed
        .set_field(
            "structuredPatch",
            Utf16JsonProjection::array(patches)
                .map_err(|_| invalid(tool, "could not construct parsed patch"))?,
        )
        .map_err(|_| invalid(tool, "could not construct parsed output"))?;
    if parsed.contains_property("gitDiff") {
        require_enum(&parsed, "/gitDiff/status", &["modified", "added"], tool)?;
    }
    Ok(parsed)
}

fn require_enum(
    output: &Utf16JsonProjection,
    pointer: &str,
    values: &[&str],
    tool: &str,
) -> Result<(), ToolError> {
    if output
        .string_at(pointer)
        .is_some_and(|value| values.contains(&value))
    {
        Ok(())
    } else {
        Err(invalid(
            tool,
            &format!("output field `{pointer}` has an invalid value"),
        ))
    }
}
