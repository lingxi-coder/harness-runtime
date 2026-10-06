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

#[test]
fn local_app_operator_schema_requires_complete_host_qa_projection() {
    let document: serde_json::Value =
        serde_json::from_str(local_app_plugin::schemas::WORKFLOW_AGENT_RESULTS)
            .expect("parse checked-in Local App workflow role schemas");
    let mut schema = document["$defs"]["operator_result"].clone();
    schema["$defs"] = document["$defs"].clone();
    let schema = serde_json::to_string(&schema).expect("serialize operator role schema");

    let complete = serde_json::json!({
        "ok": true,
        "qa_handle": "qa_00000000000000000000000000000000",
        "evidence_ids": ["evidence-1"],
        "status": "evidence_collected",
        "issues": [],
        "summary": "Host evidence collected",
        "verification_scope": {
            "declared_target_ids": ["primary", "ipad"],
            "in_scope_target_ids": ["primary"],
            "unverified_target_ids": ["ipad"],
            "unverified_scenario_ids": ["ipad-layout"]
        },
        "upstream_failures": [{
            "id": "source:save",
            "message": "save did not persist",
            "introduced_at_ms": 10
        }],
        "upstream_findings": [{
            "id": "source:save",
            "message": "save did not persist",
            "blocking": true,
            "resolved_by_evidence_ids": []
        }]
    });
    assert!(
        agent::runner::validate_structured_output(Some(&schema), &complete).is_ok(),
        "the complete Host QaBegin projection must satisfy the production validator"
    );

    let mut missing_scope = complete.clone();
    missing_scope
        .as_object_mut()
        .expect("operator result object")
        .remove("verification_scope");
    assert!(
        agent::runner::validate_structured_output(Some(&schema), &missing_scope).is_err(),
        "operator output without canonical Host scope must fail closed"
    );

    for ledger_field in ["upstream_failures", "upstream_findings"] {
        let mut missing_ledger = complete.clone();
        missing_ledger
            .as_object_mut()
            .expect("operator result object")
            .remove(ledger_field);
        assert!(
            agent::runner::validate_structured_output(Some(&schema), &missing_ledger).is_err(),
            "operator output without {ledger_field} must fail closed"
        );
    }

    let mut incomplete_finding = complete;
    incomplete_finding["upstream_findings"][0]
        .as_object_mut()
        .expect("upstream finding object")
        .remove("resolved_by_evidence_ids");
    assert!(
        agent::runner::validate_structured_output(Some(&schema), &incomplete_finding).is_err(),
        "Host upstream finding projection must include resolution evidence ids"
    );
}
