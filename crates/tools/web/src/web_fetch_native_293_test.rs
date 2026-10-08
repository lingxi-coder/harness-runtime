//! Replay the actual WebFetch output parser against public Native Mod captures.
//! The Native continuation supplies controlled results after its network call;
//! these tests validate result parsing, not WebFetch networking or worker IPC.

use super::*;

fn fixture(group: &str, case: &str, file: &str) -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../docs/parity/current-alignment/fixtures/tool-output-parser/native-baseline/2.1.293")
            .join(group)
            .join(case)
            .join(file),
    )
    .unwrap()
}

fn returned_result(group: &str, case: &str) -> Source {
    let marker = fixture(group, case, "hook-marker.raw.txt");
    let (_, source) = marker.split_once("NATIVE_WEBFETCH_OUTPUT=").unwrap();
    Source::parse(source.trim())
        .unwrap()
        .subprojection("/result")
        .unwrap()
}

#[test]
fn native_293_webfetch_strips_nested_unknown_keys_in_schema_order() {
    let group = "webfetch-nested";
    let case = "valid-unknown-keys";
    let parsed = parse_web_fetch_output(&returned_result(group, case)).unwrap();
    let native = fixture(group, case, "tool-use-result.raw.json");
    assert_eq!(
        parsed.to_json_string().unwrap().as_bytes(),
        native.as_bytes()
    );
}

#[test]
fn native_293_webfetch_nested_type_literal_and_aggregate_issue_bytes() {
    for (group, case) in [
        ("webfetch-nested", "slug-wrong-type"),
        ("webfetch-nested", "slug-missing"),
        ("webfetch-nested", "seeded-true"),
        ("webfetch-nested", "seeded-string"),
        ("webfetch-nested-followup", "artifact-read-null"),
        (
            "webfetch-nested-followup",
            "bytes-missing-slug-number-seeded-true",
        ),
    ] {
        let error = parse_web_fetch_output(&returned_result(group, case)).unwrap_err();
        let native =
            Source::parse(&fixture(group, case, "model-tool-result-content.raw.json")).unwrap();
        // Compare the fixed parser issue body. The dispatcher envelope and its
        // dynamic token reminder remain separate integration contracts.
        let body = native.string_at("").unwrap()
            .strip_prefix("<tool_use_error>tool.call step resolved WebFetch with a result that does not match its output shape: ")
            .unwrap()
            .split_once("</tool_use_error>")
            .unwrap().0;
        assert_eq!(
            error
                .model_facing_message()
                .string_at("")
                .unwrap()
                .as_bytes(),
            body.as_bytes(),
            "{case}"
        );
    }
}
