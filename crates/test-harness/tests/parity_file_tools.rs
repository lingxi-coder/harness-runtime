//! Parity fixture driver: byte-locks every M4-01 numeric/string constant
//! against the values pulled from production source.
//!
//! Spec: `docs/superpowers/specs/2026-05-24-m4-tools-implementation-design.md`
//! §6 (test target — `parity_file_tools.json` driver) + §7 (wire identifiers).

#![allow(clippy::too_many_lines, clippy::items_after_statements)]

use serde::Deserialize;
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
struct SizeLimitRow {
    value: u64,
    error_template: String,
}

#[derive(Deserialize)]
struct BinaryRow {
    scan_window_bytes: usize,
    error_template: String,
}

#[derive(Deserialize)]
struct StringRow {
    template: String,
}

#[derive(Deserialize)]
struct ValueRow<T> {
    value: T,
}

#[derive(Deserialize)]
struct OutputTruncationRow {
    max_length: usize,
    suffix: String,
}

#[derive(Deserialize)]
struct PathBlockedEventRow {
    value: String,
}

#[derive(Deserialize)]
struct TelemetryEventsRow {
    read: Vec<String>,
    write: Vec<String>,
    edit: Vec<String>,
    notebook: Vec<String>,
    // Grep and Glob emit NO telemetry (claude-code v2.1.183 emits no
    // tengu_tool_grep_* / tengu_tool_glob_* events), so there are no
    // `glob` / `grep` event lists.
}

#[derive(Deserialize)]
struct Fixture {
    #[serde(rename = "_source")]
    source: String,
    #[serde(rename = "_note")]
    note: String,
    tools: Vec<String>,
    read_size_limit: SizeLimitRow,
    binary_detection: BinaryRow,
    path_blocked_error: StringRow,
    path_blocked_event_name: PathBlockedEventRow,
    patch_truncation_suffix: StringRow,
    output_truncation: OutputTruncationRow,
    glob_cap: ValueRow<usize>,
    grep_records_cap: ValueRow<usize>,
    line_indexing_base: ValueRow<u32>,
    telemetry_events: TelemetryEventsRow,
}

#[test]
fn file_tools_fixture_matches_production_constants() {
    let fx: Fixture = load_fixture("file_tools");

    assert!(!fx.source.is_empty(), "_source missing");
    assert!(!fx.note.is_empty(), "_note missing");

    assert_eq!(
        fx.tools,
        vec!["Read", "Write", "Edit", "NotebookEdit", "Glob", "Grep"]
    );

    assert_eq!(
        fx.read_size_limit.value,
        tool_file::read::MAX_FILE_READ_SIZE
    );
    // Byte-VERBATIM to claude-code FileTooLargeError (readFileInRange.ts:62-64).
    // The runtime formatter `format_too_large` substitutes both sizes via
    // formatFileSize; cross-check the template against a concrete instance.
    assert_eq!(
        fx.read_size_limit.error_template,
        "File content ({size}) exceeds maximum allowed size ({max}). Use offset and limit parameters to read specific portions of the file, or search for specific content instead of reading the whole file."
    );
    let concrete_too_large = fx
        .read_size_limit
        .error_template
        .replace("{size}", "293KB")
        .replace("{max}", "256KB");
    assert_eq!(
        concrete_too_large,
        tool_file::read::format_too_large(std::path::Path::new("/tmp/x"), 300_000)
    );

    assert_eq!(
        fx.binary_detection.scan_window_bytes,
        tool_file::shared::NUL_SCAN_WINDOW
    );
    // Byte-VERBATIM to FileReadTool.ts:479; the lowercased extension is
    // interpolated by `format_binary`.
    assert_eq!(
        fx.binary_detection.error_template,
        "This tool cannot read binary files. The file appears to be a binary {ext} file. Please use appropriate tools for binary file analysis."
    );
    let concrete_binary = fx.binary_detection.error_template.replace("{ext}", ".bin");
    assert_eq!(
        concrete_binary,
        tool_file::read::format_binary(std::path::Path::new("/tmp/x.bin"))
    );

    assert_eq!(
        fx.path_blocked_error.template,
        "File path {path} is outside trusted directories"
    );
    assert_eq!(
        fx.path_blocked_event_name.value,
        tool_api::util::path_validation::PATH_BLOCKED_EVENT
    );
    assert_eq!(fx.path_blocked_event_name.value, "tengu_file_path_blocked");

    assert_eq!(
        fx.patch_truncation_suffix.template,
        tool_file::edit::PATCH_TRUNCATION_SUFFIX_TEMPLATE
    );
    assert_eq!(
        fx.patch_truncation_suffix.template,
        "\n\n... [{N} lines truncated] ..."
    );

    assert_eq!(
        fx.output_truncation.max_length,
        tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH
    );
    assert_eq!(fx.output_truncation.max_length, 30_000);
    assert_eq!(
        fx.output_truncation.suffix,
        tool_api::util::output_truncation::SHELL_TRUNCATION_SUFFIX_TEMPLATE
    );
    assert_eq!(
        fx.output_truncation.suffix,
        "\n\n... [{N} lines truncated] ..."
    );

    assert_eq!(fx.glob_cap.value, tool_file::glob::MAX_GLOB_MATCHES);
    assert_eq!(fx.glob_cap.value, 100);

    // Per-file match cap removed for claude-code/rg parity (no per-file cap);
    // GREP_RECORDS_CAP is now a memory valve on recorded lines only — it never
    // caps the count. head_limit (default 250) is the real truncation.
    assert_eq!(fx.grep_records_cap.value, tool_file::grep::GREP_RECORDS_CAP);
    assert_eq!(fx.grep_records_cap.value, 10_000);

    assert_eq!(fx.line_indexing_base.value, 1);

    use telemetry::tengu::tool as ev;
    assert_eq!(
        fx.telemetry_events.read,
        vec![
            ev::READ_STARTED.to_string(),
            ev::READ_COMPLETED.to_string(),
            ev::READ_FAILED.to_string()
        ]
    );
    assert_eq!(
        fx.telemetry_events.write,
        vec![
            ev::WRITE_STARTED.to_string(),
            ev::WRITE_COMPLETED.to_string(),
            ev::WRITE_FAILED.to_string()
        ]
    );
    assert_eq!(
        fx.telemetry_events.edit,
        vec![
            ev::EDIT_STARTED.to_string(),
            ev::EDIT_COMPLETED.to_string(),
            ev::EDIT_FAILED.to_string()
        ]
    );
    assert_eq!(
        fx.telemetry_events.notebook,
        vec![
            ev::NOTEBOOK_STARTED.to_string(),
            ev::NOTEBOOK_COMPLETED.to_string(),
            ev::NOTEBOOK_FAILED.to_string()
        ]
    );
    // Grep and Glob emit NO telemetry (claude-code v2.1.183 emits no
    // tengu_tool_grep_* / tengu_tool_glob_* events), so there is nothing to
    // assert for them here.
}
