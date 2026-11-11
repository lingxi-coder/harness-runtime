//! Differential coverage and a manual scaling probe for NotebookEdit.

use super::*;

// The pre-optimization algorithm is intentionally retained only as a test oracle.
fn legacy_joined_cell_source(cell: &Utf16JsonProjection) -> Result<Utf16JsonProjection, ToolError> {
    if cell.string_units("/source").is_some() {
        return cell
            .subprojection("/source")
            .map_err(|_| notebook_error(INVALID_NOTEBOOK));
    }
    let count = cell
        .array_len("/source")
        .ok_or_else(|| notebook_error(INVALID_NOTEBOOK))?;
    let mut units = Vec::new();
    for index in 0..count {
        units.extend(
            cell.string_units(&format!("/source/{index}"))
                .ok_or_else(|| notebook_error(INVALID_NOTEBOOK))?,
        );
    }
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .map_err(|_| notebook_error(INVALID_NOTEBOOK))
}

fn legacy_mutate_notebook(
    notebook: &mut Utf16JsonProjection,
    input: &Utf16JsonProjection,
    cell_id: Option<&str>,
    edit_mode: &str,
    cell_type: Option<&str>,
    insert_id: impl FnOnce() -> String,
) -> Result<NotebookMutation, ToolError> {
    let count = notebook
        .array_len("/cells")
        .ok_or_else(|| notebook_error(INVALID_NOTEBOOK))?;
    let mut cells = (0..count)
        .map(|index| {
            let cell = notebook
                .subprojection(&format!("/cells/{index}"))
                .map_err(|_| notebook_error(INVALID_NOTEBOOK))?;
            if cell.object_entries().is_none() {
                return Err(notebook_error(INVALID_NOTEBOOK));
            }
            legacy_joined_cell_source(&cell)?;
            Ok(cell)
        })
        .collect::<Result<Vec<_>, ToolError>>()?;
    let cell_id = cell_id.filter(|id| !id.is_empty());
    let index = match cell_id {
        None if edit_mode == EDIT_MODE_INSERT => 0,
        None => {
            return Err(notebook_error(
                "Cell ID must be specified when not inserting a new cell.",
            ));
        }
        Some(id) => {
            let found = cells
                .iter()
                .position(|cell| cell.string_at("/id") == Some(id));
            let index = if let Some(index) = found {
                index
            } else if let Some(index) = parse_cell_id(id) {
                if index >= count {
                    return Err(notebook_error(format!(
                        "Cell with index {index} does not exist in notebook."
                    )));
                }
                index
            } else {
                return Err(notebook_error(format!(
                    "Cell with ID \"{id}\" not found in notebook."
                )));
            };
            index + usize::from(edit_mode == EDIT_MODE_INSERT)
        }
    };
    let new_source = input
        .subprojection("/new_source")
        .ok()
        .filter(|source| source.string_units("").is_some())
        .unwrap_or_else(|| "".into());
    let language = notebook
        .subprojection("/metadata/language_info/name")
        .ok()
        .filter(|name| !name.at_pointer("").is_some_and(|value| value.is_null()))
        .unwrap_or_else(|| "python".into());
    let supports_ids = notebook.number_at("/nbformat").is_some_and(|format| {
        format > 4.0
            || (format == 4.0
                && notebook
                    .number_at("/nbformat_minor")
                    .is_some_and(|minor| minor >= 5.0))
    });
    let result_id = if edit_mode == EDIT_MODE_INSERT {
        supports_ids.then(|| insert_id().chars().take(8).collect::<String>())
    } else {
        cell_id.map(str::to_owned)
    };
    let old_source = match edit_mode {
        EDIT_MODE_DELETE => Some(legacy_joined_cell_source(&cells.remove(index))?),
        EDIT_MODE_INSERT => {
            let kind = cell_type.unwrap_or("code");
            let mut fields = vec![("cell_type".into(), kind.into())];
            if let Some(id) = &result_id {
                fields.push(("id".into(), id.as_str().into()));
            }
            fields.extend([
                ("source".into(), new_source.clone()),
                ("metadata".into(), crate::generated_result(&json!({}))),
            ]);
            if kind != "markdown" {
                fields.extend([
                    (
                        "execution_count".into(),
                        crate::generated_result(&Value::Null),
                    ),
                    ("outputs".into(), crate::generated_result(&json!([]))),
                ]);
            }
            cells.insert(
                index,
                Utf16JsonProjection::object(fields)
                    .map_err(|error| notebook_error(error.to_string()))?,
            );
            None
        }
        _ => {
            let cell = &mut cells[index];
            let old_source = legacy_joined_cell_source(cell)?;
            if cell.string_at("/cell_type") == Some("code") {
                cell.set_field("execution_count", crate::generated_result(&Value::Null))
                    .map_err(|error| notebook_error(error.to_string()))?;
                cell.set_field("outputs", crate::generated_result(&json!([])))
                    .map_err(|error| notebook_error(error.to_string()))?;
            }
            cell.set_field("source", new_source.clone())
                .map_err(|error| notebook_error(error.to_string()))?;
            if let Some(kind) = cell_type {
                cell.set_field("cell_type", kind.into())
                    .map_err(|error| notebook_error(error.to_string()))?;
            }
            Some(old_source)
        }
    };
    notebook
        .set_field(
            "cells",
            Utf16JsonProjection::array(cells).map_err(|error| notebook_error(error.to_string()))?,
        )
        .map_err(|error| notebook_error(error.to_string()))?;
    let display_id = result_id.as_deref().unwrap_or("undefined");
    let prefix = match edit_mode {
        EDIT_MODE_DELETE => format!("Deleted cell {display_id}"),
        EDIT_MODE_INSERT if result_id.is_none() => "Inserted a new cell with ".into(),
        EDIT_MODE_INSERT => format!("Inserted cell {display_id} with "),
        _ => format!("Updated cell {display_id} with "),
    };
    let mut units: Vec<u16> = prefix.encode_utf16().collect();
    if edit_mode != EDIT_MODE_DELETE {
        units.extend(new_source.string_units("").unwrap_or_default());
    }
    let model_content = Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .map_err(|error| notebook_error(error.to_string()))?;
    Ok(NotebookMutation {
        new_source,
        old_source,
        language,
        cell_id: result_id.map(Into::into),
        model_content,
    })
}

fn mutation_bytes(mutation: &NotebookMutation) -> Vec<String> {
    vec![
        mutation.new_source.to_json_string().unwrap(),
        mutation
            .old_source
            .as_ref()
            .map(|value| value.to_json_string().unwrap())
            .unwrap_or_else(|| "<absent>".into()),
        mutation.language.to_json_string().unwrap(),
        format!("{:?}", mutation.cell_id),
        mutation.model_content.to_json_string().unwrap(),
    ]
}

fn assert_same_mutation(raw: &str, id: Option<&str>, mode: &str, kind: Option<&str>) {
    let input = Utf16JsonProjection::parse(r#"{"new_source":"new\ud800x\udc00\ntext"}"#).unwrap();
    let mut legacy = Utf16JsonProjection::parse(raw).unwrap();
    let mut optimized = legacy.clone();
    let before = legacy_mutate_notebook(&mut legacy, &input, id, mode, kind, || {
        "12345678-rest".into()
    });
    let after = mutate_notebook(&mut optimized, &input, id, mode, kind, || {
        "12345678-rest".into()
    });
    match (before, after) {
        (Ok(before), Ok(after)) => assert_eq!(
            mutation_bytes(&after),
            mutation_bytes(&before),
            "id={id:?}, mode={mode}, notebook={raw}"
        ),
        (Err(before), Err(after)) => assert_eq!(
            after.model_facing_message().to_json_string().unwrap(),
            before.model_facing_message().to_json_string().unwrap(),
            "id={id:?}, mode={mode}, notebook={raw}"
        ),
        (before, after) => panic!(
            "different success/error result: legacy={}, optimized={}",
            before.is_ok(),
            after.is_ok()
        ),
    }
    assert_eq!(
        serialize_notebook(&optimized).unwrap(),
        serialize_notebook(&legacy).unwrap(),
        "id={id:?}, mode={mode}, notebook={raw}"
    );
}

#[test]
fn borrowed_notebook_mutation_matches_legacy_bytes_and_errors() {
    let notebooks = [
        r#"{"cells":[],"metadata":{},"nbformat":4,"nbformat_minor":4}"#,
        r#"{"9":-0.0,"2":1e-7,"cells":[{"id":"same","cell_type":"code","source":["a\ud800","x\udc00"],"outputs":[{"text":"old"}],"metadata":{"\ud800":"key"}},{"id":"same","cell_type":"markdown","source":"second"},{"id":"cell-1","cell_type":"raw","source":[]}],"metadata":{"language_info":{"name":null}},"nbformat":4,"nbformat_minor":5}"#,
        r#"{"cells":[{"source":"","cell_type":"code"},{"id":"c1","source":["one","two"],"cell_type":"raw","execution_count":2,"outputs":[1]}],"metadata":{"language_info":{"name":{"7":"kept","1":-0}}},"nbformat":5}"#,
        r#"{"cells":[{"id":"c1","source":"first","cell_type":"code"},{"id":"c2","source":null}],"nbformat":4,"nbformat_minor":5}"#,
        r#"{"cells":[{"id":"c1","source":["first",2]}]}"#,
        r#"{"cells":[null]}"#,
        r#"{"cells":[{"id":"c1"}]}"#,
        r#"{"cells":{}}"#,
        r#"{}"#,
    ];
    for raw in notebooks {
        for id in [
            None,
            Some(""),
            Some("same"),
            Some("c1"),
            Some("cell-0"),
            Some("cell-1"),
            Some("cell-999"),
            Some("missing"),
        ] {
            for mode in [EDIT_MODE_REPLACE, EDIT_MODE_INSERT, EDIT_MODE_DELETE] {
                for kind in [None, Some("markdown"), Some("code")] {
                    assert_same_mutation(raw, id, mode, kind);
                }
            }
        }
    }
}

#[test]
#[ignore = "manual scaling probe; prints timings without timing assertions"]
fn notebook_mutation_projection_scaling_probe() {
    for count in [128usize, 512, 2048] {
        let cells: Vec<_> = (0..count).map(|index| json!({"id":format!("c{index}"),"cell_type":"code","source":["print(1)\n"],"outputs":[],"metadata":{}})).collect();
        let raw = json!({"cells":cells,"metadata":{},"nbformat":4,"nbformat_minor":5}).to_string();
        let mut legacy = Utf16JsonProjection::parse(&raw).unwrap();
        let mut optimized = legacy.clone();
        let input = Utf16JsonProjection::parse(r#"{"new_source":"changed"}"#).unwrap();
        let started = Instant::now();
        let before = legacy_mutate_notebook(
            &mut legacy,
            &input,
            Some("c0"),
            EDIT_MODE_REPLACE,
            None,
            || unreachable!(),
        )
        .unwrap();
        let legacy_us = started.elapsed().as_micros();
        let started = Instant::now();
        let after = mutate_notebook(
            &mut optimized,
            &input,
            Some("c0"),
            EDIT_MODE_REPLACE,
            None,
            || unreachable!(),
        )
        .unwrap();
        let optimized_us = started.elapsed().as_micros();
        assert_eq!(mutation_bytes(&after), mutation_bytes(&before));
        assert_eq!(
            serialize_notebook(&optimized).unwrap(),
            serialize_notebook(&legacy).unwrap()
        );
        println!(
            "{{\"probe\":\"notebook_mutation\",\"cells\":{count},\"legacy_cell_subprojections\":{count},\"optimized_changed_cells_materialized\":1,\"optimized_unchanged_cells_imported\":{},\"legacy_us\":{legacy_us},\"optimized_us\":{optimized_us}}}",
            count - 1
        );
    }
}
