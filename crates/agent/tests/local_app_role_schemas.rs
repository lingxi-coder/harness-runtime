//! Regression tests for local app role schemas.

use serde_json::{json, Value};

fn role_schema(role: &str) -> Value {
    let document: Value = serde_json::from_str(local_app_plugin::schemas::WORKFLOW_AGENT_RESULTS)
        .expect("valid checked-in role schemas");
    let mut schema = document["$defs"][role].clone();
    schema["$defs"] = document["$defs"].clone();
    schema
}

#[test]
fn local_app_role_schemas_declare_object_tool_parameters() {
    // These definitions become StructuredOutput tool input schemas. Providers
    // require an explicit root object type even when all anyOf branches are objects.
    for role in [
        "operator_result",
        "qa_review",
        "qa_finalize",
        "mcp_promoter",
    ] {
        assert_eq!(role_schema(role)["type"], "object", "{role}");
    }
}

#[test]
fn local_app_union_schemas_preserve_success_and_failure_constraints() {
    for (role, valid, invalid) in [
        (
            "operator_result",
            vec![json!({"ok": false, "error": "Host unavailable"})],
            vec![
                json!({"ok": true, "error": "Host unavailable"}),
                json!({"ok": false}),
            ],
        ),
        (
            "qa_finalize",
            vec![
                json!({"ok": false, "error": "Host unavailable"}),
                json!({
                    "status": "evidence_resample_required",
                    "qa_handle": "qa_00000000000000000000000000000000",
                    "findings": [], "summary": "Collect missing evidence"
                }),
            ],
            vec![
                json!({"ok": true, "status": "candidate"}),
                json!({"ok": false}),
            ],
        ),
    ] {
        let mut schemas = boon::Schemas::new();
        let mut compiler = boon::Compiler::new();
        let url = "mem://local-app-role";
        compiler.add_resource(url, role_schema(role)).unwrap();
        let schema = compiler.compile(url, &mut schemas).unwrap();
        for value in valid {
            assert!(schemas.validate(&value, schema).is_ok(), "{role}: {value}");
        }
        for value in invalid
            .into_iter()
            .chain([json!(null), json!([]), json!({})])
        {
            assert!(schemas.validate(&value, schema).is_err(), "{role}: {value}");
        }
    }
}
