//! 校验仓库里签入的 plugin workflow 脚本能通过**运行时真正用的**那两个校验器。
//!
//! ## 为什么这个测试住在 `workflow` crate 里
//!
//! `validate_meta` 和 `check_determinism` 是这里的 `pub fn`，而被校验的脚本是
//! `plugins/lingxi-local-app/workflows/*.js`。放在别处就得把校验器再导出
//! 一层，或者复制一份判据——而判据复制出第二份的那一刻，它就开始漂移了。
//!
//! ## 为什么它必须存在（这不是假设，是已经发生过一次的事）
//!
//! 这些脚本第一次交付时，**全部通不过 `validate_meta`**：
//!
//! ```text
//! meta must be a pure literal: non-literal node type in meta: BinaryExpression
//! ```
//!
//! 原因是 `description` 写成了 `'…' + '…'` 的多行字符串拼接。那是**语法完全合法
//! 的 JavaScript**，所以 `node --check` 都是绿的——交付报告里也确实写了
//! 「all scripts pass `node --check`」。
//!
//! ⛔ 所以 `node --check` 对 workflow 脚本是**假绿**。判据是这两个函数，不是
//! 语法解析器。
#![allow(clippy::needless_raw_string_hashes)]

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

fn run_with_host<R, P>(
    script: &str,
    mut agent_runner: R,
    progress: P,
    budget: Option<Arc<dyn workflow::WorkflowBudgetSource>>,
    allow_nested: bool,
    args: Option<String>,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<workflow::RunOutcome, workflow::WorkflowError>
where
    R: FnMut(&[String], &[String]) -> Vec<String> + 'static,
    P: FnMut(&workflow::Progress) + 'static,
{
    workflow::run_with_progress(
        script,
        move |prompts, options| agent_runner(prompts, options),
        progress,
        budget,
        allow_nested,
        args,
        cancel,
    )
}

/// 签入的 plugin workflow 脚本目录。
fn workflow_dir() -> PathBuf {
    local_app_builder_plugin::root().join("workflows")
}

fn authoring_spec() -> serde_json::Value {
    serde_json::from_str(local_app_builder_plugin::fixtures::AUTHORING_SPEC_VALID_NULL_CANVAS)
        .expect("parse the Host-valid checked-in AuthoringSpec fixture")
}

fn workflow_schemas() -> serde_json::Value {
    let path = workflow_dir()
        .parent()
        .expect("plugin root")
        .join("schemas/workflow-agent-results.schema.json");
    let document: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&path).expect("read checked-in workflow-agent-results schema"),
    )
    .expect("parse checked-in workflow-agent-results schema");
    let defs = document["$defs"]
        .as_object()
        .expect("workflow-agent-results schema must expose role definitions")
        .clone();
    let mut schemas = serde_json::Map::new();
    for name in [
        "operator_result",
        "qa_review",
        "qa_finalize",
        "mcp_promoter",
    ] {
        let mut role = defs
            .get(name)
            .cloned()
            .unwrap_or_else(|| panic!("missing role schema {name}"));
        role["$defs"] = serde_json::Value::Object(defs.clone());
        schemas.insert(name.to_string(), role);
    }
    schemas.insert(
        "mcp_proposal".into(),
        serde_json::from_str(
            &std::fs::read_to_string(
                workflow_dir()
                    .parent()
                    .expect("plugin root")
                    .join("schemas/mcp-proposal.schema.json"),
            )
            .expect("read checked-in MCP proposal schema"),
        )
        .expect("parse checked-in MCP proposal schema"),
    );
    serde_json::Value::Object(schemas)
}

/// r2-tests-honesty-011: blank out every `//`-comment line before running the
/// substring checks below, so a comment merely MENTIONING `agent(`,
/// `lingxi-local-app:` or `HOST_PRECONDITION_UNAVAILABLE` can no longer
/// satisfy (or defeat) them the way live code would.
fn strip_line_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("//") {
                ""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn use_test_args(quality: &str) -> serde_json::Value {
    serde_json::json!({
        "app_id": "aaaa1111",
        "scope": "acceptance",
        "scenarios": ["primary-action"],
        "quality_level": quality,
        "workflow_run_id": "wf_use_test",
        "runtime_profile": {
            "family": "react_dom",
            "revision": 1,
            "contract_sha256": "a".repeat(64)
        },
        "host_context": {
            "source": "verified_host",
            "app_id": "aaaa1111",
            "workflow_run_id": "wf_use_test",
            "schemas": workflow_schemas(),
            "runtime_profile": {
                "family": "react_dom",
                "revision": 1,
                "contract_sha256": "a".repeat(64),
                "surface": "dom"
            },
            "authoring_spec": authoring_spec()
        }
    })
}

/// Raw evidence identity returned by the non-judging operator.
fn operator_report(qa_handle: &str) -> serde_json::Value {
    serde_json::json!({
        "qa_handle": qa_handle,
        "evidence_ids": ["native-1"],
        "status": "evidence_collected",
        "issues": [],
        "verification_scope": {
            "declared_target_ids": ["primary"],
            "in_scope_target_ids": ["primary"],
            "unverified_target_ids": [],
            "unverified_scenario_ids": []
        },
        "upstream_failures": [],
        "upstream_findings": [],
        "summary": "Host evidence collected"
    })
}

fn partial_operator_report(qa_handle: &str) -> serde_json::Value {
    let mut report = operator_report(qa_handle);
    report["verification_scope"] = serde_json::json!({
        "declared_target_ids": ["primary", "ipad"],
        "in_scope_target_ids": ["primary"],
        "unverified_target_ids": ["ipad"],
        "unverified_scenario_ids": ["ipad-layout"]
    });
    report["upstream_failures"] = serde_json::json!([{
        "id": "environment:ipad-unavailable",
        "message": "the iPad device is unavailable",
        "introduced_at_ms": 10
    }]);
    report["upstream_findings"] = serde_json::json!([{
        "id": "environment:ipad-unavailable",
        "message": "the iPad device is unavailable",
        "blocking": true,
        "resolved_by_evidence_ids": []
    }]);
    report
}

fn qa_candidate_report(
    workflow_run_id: &str,
    quality: &str,
    passed: bool,
    verifier: bool,
    qa_handle: &str,
) -> serde_json::Value {
    let result_id = if verifier {
        "qa-result-verifier"
    } else {
        "qa-result-tester"
    };
    let findings = if passed {
        serde_json::json!([])
    } else {
        serde_json::json!([{
            "id": "source:acceptance-save",
            "message": "the save button does nothing",
            "blocking": true,
            "resolved_by_evidence_ids": []
        }])
    };
    let mut result: serde_json::Value =
        serde_json::from_str(local_app_builder_plugin::fixtures::QA_RESULT_VALID)
            .expect("parse the Host-valid checked-in QA result fixture");
    result["identity"]["app_id"] = serde_json::Value::String("aaaa1111".into());
    result["identity"]["workflow_run_id"] = serde_json::Value::String(workflow_run_id.into());
    result["identity"]["qa_handle"] = serde_json::Value::String(qa_handle.into());
    result["identity"]["verification_strategy"] = serde_json::Value::String(quality.into());
    result["scenario_judgements"][0]["status"] =
        serde_json::Value::String(if passed { "passed" } else { "failed" }.into());
    result["scenario_judgements"][0]["summary"] =
        serde_json::Value::String(if passed { "Passed" } else { "Failed" }.into());
    result["findings"] = findings;
    result["previous_result_id"] = if verifier {
        serde_json::Value::String("qa-result-tester".into())
    } else {
        serde_json::Value::Null
    };
    result["finalized_at_ms"] = serde_json::Value::from(if verifier { 3 } else { 2 });
    let result_sha256 = result["result_sha256"]
        .as_str()
        .expect("fixture result digest")
        .to_string();
    serde_json::json!({
        // Deliberately opposite on passing results: workflow success must come
        // from the Host candidate, never this model-facing convenience flag.
        "ok": !passed,
        "status": "candidate",
        "receipt": {
            "receipt_id": format!("receipt-{result_id}"),
            "app_id": "aaaa1111",
            "workflow_run_id": workflow_run_id,
            "qa_handle": qa_handle,
            "result_id": result_id,
            "identity_sha256": "a".repeat(64),
            "result_sha256": result_sha256,
            "issued_at_ms": 2
        },
        "result": result
    })
}

fn passing_qa_reply(label: &str, workflow_run_id: &str, quality: &str) -> String {
    let qa_handle = qa_handle_for_label(label);
    if label.starts_with("operator-") {
        operator_report(&qa_handle).to_string()
    } else {
        qa_candidate_report(
            workflow_run_id,
            quality,
            true,
            label.starts_with("verifier"),
            &qa_handle,
        )
        .to_string()
    }
}

fn failing_qa_reply(label: &str, workflow_run_id: &str, quality: &str) -> String {
    let qa_handle = qa_handle_for_label(label);
    if label.starts_with("operator-") {
        operator_report(&qa_handle).to_string()
    } else {
        qa_candidate_report(
            workflow_run_id,
            quality,
            false,
            label.starts_with("verifier"),
            &qa_handle,
        )
        .to_string()
    }
}

fn qa_handle_for_label(label: &str) -> String {
    if label.contains("resample") {
        return "qa_11111111111111111111111111111111".into();
    }
    let round = label
        .rsplit_once('-')
        .and_then(|(_, suffix)| suffix.parse::<usize>().ok())
        .unwrap_or(0);
    if round == 0 {
        "qa_00000000000000000000000000000000".into()
    } else {
        format!("qa_{round:032}")
    }
}

fn stage_label(options: &str) -> String {
    serde_json::from_str::<serde_json::Value>(options)
        .unwrap_or(serde_json::Value::Null)
        .get("label")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}

#[test]
fn verification_scope_schema_rejects_empty_required_target_lists() {
    let workflow_root = workflow_dir();
    let plugin_root = workflow_root.parent().expect("plugin root");
    let invalid_fixture: serde_json::Value =
        serde_json::from_str(local_app_builder_plugin::fixtures::QA_RESULT_INVALID_EMPTY_SCOPE)
            .expect("parse the negative empty verification scope fixture");
    let fixture_scope = invalid_fixture["verification_scope"]
        .as_object()
        .expect("negative fixture verification scope");

    for schema_name in [
        "schemas/workflow-agent-results.schema.json",
        "schemas/qa-report.schema.json",
        "schemas/use-test-report.schema.json",
    ] {
        let schema: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(plugin_root.join(schema_name))
                .unwrap_or_else(|error| panic!("read {schema_name}: {error}")),
        )
        .unwrap_or_else(|error| panic!("parse {schema_name}: {error}"));
        let properties = schema["$defs"]["verification_scope"]["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{schema_name} verification scope properties"));

        for field in ["declared_target_ids", "in_scope_target_ids"] {
            assert_eq!(
                properties[field]["minItems"].as_u64(),
                Some(1),
                "{schema_name} must reject an empty {field}"
            );
            assert!(
                fixture_scope[field]
                    .as_array()
                    .expect("negative fixture target list")
                    .is_empty(),
                "negative fixture must exercise the empty {field} case"
            );
        }
        for field in ["unverified_target_ids", "unverified_scenario_ids"] {
            assert_eq!(
                properties[field].get("minItems"),
                None,
                "{schema_name} must allow an empty {field}"
            );
            assert!(
                fixture_scope[field]
                    .as_array()
                    .expect("negative fixture unverified list")
                    .is_empty(),
                "negative fixture should keep {field} empty"
            );
        }
    }
}

#[test]
fn every_checked_in_plugin_workflow_passes_the_runtime_validators() {
    let dir = workflow_dir();
    assert!(
        dir.is_dir(),
        "plugin workflow directory missing at {} — refusing to report a clean result from a \
         directory that does not exist",
        dir.display()
    );

    let mut scripts: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read workflow dir")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "js"))
        .collect();
    scripts.sort();

    // Fail closed. 一个扫到零个文件的门和没有门是同一件事,但它看起来像通过了。
    assert!(
        !scripts.is_empty(),
        "enumerated ZERO .js workflow scripts under {} — refusing to report clean from an empty \
         enumeration",
        dir.display()
    );

    let mut failures = Vec::new();
    for script in &scripts {
        let name = script
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let src = match std::fs::read_to_string(script) {
            Ok(src) => src,
            Err(error) => {
                failures.push(format!("{name}: unreadable ({error})"));
                continue;
            }
        };
        if let Err(error) = workflow::validate_meta(&src) {
            failures.push(format!(
                "{name}: validate_meta rejected it — {error:?}. `meta` must be a PURE LITERAL; \
                 a concatenated description (`'a' + 'b'`) is valid JavaScript and passes \
                 `node --check`, but parses as a BinaryExpression and is refused here."
            ));
        }
        if let Err(error) = workflow::check_determinism(&src) {
            failures.push(format!(
                "{name}: check_determinism rejected it — {error:?}. Date.now(), Math.random() \
                 and zero-arg new Date() break resume, because the journal replays prior agent() \
                 results but re-runs the JS body."
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} checked-in plugin workflow script(s) fail the runtime validators:\n  - {}",
        failures.len(),
        scripts.len(),
        failures.join("\n  - ")
    );
}

#[test]
fn phase4_and_phase6_workflows_use_real_orchestration() {
    let use_test = std::fs::read_to_string(workflow_dir().join("local-app-use-test.js"))
        .expect("read use-test workflow");
    {
        let (name, source) = ("local-app-use-test.js", use_test);
        let code = strip_line_comments(&source);
        assert!(
            code.contains("agent("),
            "{name} must orchestrate Plugin agents"
        );
        // Not a bare substring: the namespaced identity must be the script's
        // OWN declared `WORKFLOW_ID`, not merely a string that happens to
        // appear somewhere (a comment mentioning the namespace used to be
        // enough to satisfy this).
        assert!(
            code.lines().any(|line| line
                .trim_start()
                .starts_with("const WORKFLOW_ID = 'lingxi-local-app:")),
            "{name} must declare `const WORKFLOW_ID = 'lingxi-local-app:...'`, not merely \
             mention the namespace"
        );
        assert!(
            !code.contains("HOST_PRECONDITION_UNAVAILABLE"),
            "{name} must no longer be a Phase2 placeholder"
        );
    }
    let mcp = std::fs::read_to_string(workflow_dir().join("local-app-mcp-authoring.js"))
        .expect("read mcp workflow");
    let mcp_code = strip_line_comments(&mcp);
    assert!(
        mcp_code.contains("agent("),
        "Phase6 MCP authoring must orchestrate agents"
    );
    assert!(
        mcp_code.contains("mcp_authoring_required"),
        "zero-tool authoring must be explicit"
    );
    assert!(
        !mcp_code.contains("HOST_PRECONDITION_UNAVAILABLE"),
        "Phase6 MCP authoring must no longer be a placeholder"
    );
}

#[test]
fn mcp_authoring_workflow_validates_zero_tool_candidates_and_executes_approval_path() {
    let source = std::fs::read_to_string(workflow_dir().join("local-app-mcp-authoring.js"))
        .expect("read mcp workflow");
    let initial_args = serde_json::json!({
        "app_id": "aaaa1111",
        "user_goal": "Expose the saved recipes search as MCP",
        "workflow_run_id": "wf_mcp_authoring1",
        "runtime_profile": {"family":"react_dom","revision":1,"contract_sha256":"a".repeat(64),"surface":"dom"},
        "expected_writable_collections": ["recipes"],
        "host_context": {
            "source": "verified_host",
            "operation": "initial",
            "app_id": "aaaa1111",
            "workflow_run_id": "wf_mcp_authoring1",
            "invocation_capability": "mcpv_00000000000000000000000000000000",
            "schemas": workflow_schemas(),
            "template_catalog": {"catalog_digest": "digest", "available_template_ids": ["react-dom-r2"]},
            "expected_writable_collections": ["recipes"],
            "dependency_snapshot": {"verified": true},
            "active_catalog": null
        }
    });
    let revise_args = serde_json::json!({
        "app_id": "aaaa1111",
        "user_goal": "Expose the saved recipes search as MCP",
        "workflow_run_id": "wf_mcp_authoring2",
        "runtime_profile": {"family":"react_dom","revision":1,"contract_sha256":"a".repeat(64),"surface":"dom"},
        "expected_writable_collections": ["recipes"],
        "host_context": {
            "source": "verified_host",
            "operation": "revise",
            "app_id": "aaaa1111",
            "workflow_run_id": "wf_mcp_authoring2",
            "invocation_capability": "mcpv_00000000000000000000000000000000",
            "schemas": workflow_schemas(),
            "template_catalog": {"catalog_digest": "digest", "available_template_ids": ["react-dom-r2"]},
            "expected_writable_collections": ["recipes"],
            "dependency_snapshot": {"verified": true},
            "active_catalog": {"build_id":"build-a","manifest_revision":3,"authoring_revision":1,"catalog_sha256":"9".repeat(64),"approval_contract_sha256":"a".repeat(64),"tool_surface_sha256":"b".repeat(64)}
        }
    });
    let evidence = serde_json::json!({
        "summary": "search UI with persisted recipes collection",
        "runtime_profile": {"family": "react_dom"},
        "collections": ["recipes"],
        "active_catalog": null
    });
    let proposal = serde_json::json!({
        "appId": "aaaa1111",
        "manifestRevision": 3,
        "userGoalSha256": "0000000000000000000000000000000000000000000000000000000000000000",
        "summary": "Expose bounded recipe search",
        "tools": [{
            "name": "search_recipes",
            "title": "Search recipes",
            "description": "Search saved recipes",
            "inputSchema": {"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false},
            "outputSchema": {"type":"object","properties":{"items":{"type":"array"}},"required":["items"],"additionalProperties":false},
            "semanticFlowId": "search_recipes",
            "inputs": {"query":{"tool_input":{"json_pointer":"/query"}}},
            "result": {"step_output":{"step_id":"search","json_pointer":"/items"}}
        }],
        "requiredFlowChanges": [],
        "excludedCapabilities": []
    });
    let validated = serde_json::json!({
        "ok": true,
        "status": "approval_required",
        "proposal_sha256": "1".repeat(64),
        "approval_contract_sha256": "2".repeat(64),
        "tool_surface_sha256": "3".repeat(64),
        "findings": []
    });
    let approved = serde_json::json!({
        "approved": true,
        "receipt_id": "mrcpt_00000000000000000000000000000000",
        "status": "approved"
    });
    let qa = serde_json::json!({
        "ok": true,
        "findings": [],
        "mcp_schema": "passed",
        "flow_binding": "passed",
        "calls": "passed",
        "isolation": "passed",
        "verification_sha256": "4".repeat(64),
        "summary": "passed"
    });
    let promoted = serde_json::json!({
        "promoted": true,
        "catalog_sha256": "5".repeat(64),
        "status": "promoted",
        "publication_state": "published_unverified"
    });
    let approval_calls = Arc::new(AtomicUsize::new(0));
    let approval_calls_for_run = Arc::clone(&approval_calls);
    let evidence_for_promote = evidence.clone();
    let proposal_for_initial = proposal.clone();
    let validated_for_initial = validated.clone();
    let approved_for_initial = approved.clone();
    let qa_for_initial = qa.clone();
    let promoted_for_initial = promoted.clone();
    let promotion_prompt = Arc::new(Mutex::new(String::new()));
    let promotion_prompt_for_run = Arc::clone(&promotion_prompt);
    let outcome = run_with_host(
        &source,
        move |prompts, options| {
            options
                .iter()
                .zip(prompts.iter())
                .map(|(options, prompt)| {
                    if options.contains("app-evidence") {
                        evidence.to_string()
                    } else if options.contains("host-proposal-validation") {
                        validated_for_initial.to_string()
                    } else if options.contains("native-approval") {
                        approval_calls_for_run.fetch_add(1, Ordering::SeqCst);
                        approved_for_initial.to_string()
                    } else if options.contains("mcp-qa") {
                        qa_for_initial.to_string()
                    } else if options.contains("mcp-promote") {
                        *promotion_prompt_for_run.lock().expect("promotion prompt") =
                            prompt.clone();
                        promoted_for_initial.to_string()
                    } else {
                        proposal_for_initial.to_string()
                    }
                })
                .collect()
        },
        |_progress| {},
        None,
        false,
        Some(initial_args.to_string()),
        None,
    )
    .expect("mcp authoring workflow should execute");
    let result = outcome.result.expect("workflow result");
    assert!(result.contains("\"status\":\"promoted\""));
    assert_eq!(approval_calls.load(Ordering::SeqCst), 1);
    let promotion_prompt = promotion_prompt.lock().expect("promotion prompt");
    assert!(
        promotion_prompt.contains(
            r#"{"app_id":"aaaa1111","workflow_run_id":"wf_mcp_authoring1","receipt_id":"mrcpt_00000000000000000000000000000000"}"#
        ),
        "promotion must use only the real tool input fields: {promotion_prompt}"
    );
    for forbidden_payload in [
        "1111111111111111",
        "4444444444444444",
        "tool_surface_sha256",
    ] {
        assert!(
            !promotion_prompt.contains(forbidden_payload),
            "promotion prompt leaked non-input candidate/QA data {forbidden_payload}: {promotion_prompt}"
        );
    }

    let evidence_for_zero_tools = evidence_for_promote.clone();
    let proposal_for_revise = proposal.clone();
    let mut validated_for_revise = validated.clone();
    validated_for_revise["status"] = serde_json::Value::String("approved_reusable".into());
    let qa_for_revise = qa.clone();
    let promoted_for_revise = promoted.clone();
    let reuse_promotion_prompt = Arc::new(Mutex::new(String::new()));
    let reuse_promotion_prompt_for_run = Arc::clone(&reuse_promotion_prompt);
    let revise_outcome = run_with_host(
        &source,
        move |prompts, options| {
            options
                .iter()
                .zip(prompts.iter())
                .map(|(options, prompt)| {
                    if options.contains("app-evidence") {
                        evidence_for_promote.to_string()
                    } else if options.contains("host-proposal-validation") {
                        validated_for_revise.to_string()
                    } else if options.contains("native-approval") {
                        panic!("approved_reusable must not request a second native approval")
                    } else if options.contains("mcp-qa") {
                        qa_for_revise.to_string()
                    } else if options.contains("mcp-promote") {
                        *reuse_promotion_prompt_for_run
                            .lock()
                            .expect("reuse promotion prompt") = prompt.clone();
                        promoted_for_revise.to_string()
                    } else {
                        proposal_for_revise.to_string()
                    }
                })
                .collect()
        },
        |_progress| {},
        None,
        false,
        Some(revise_args.to_string()),
        None,
    )
    .expect("revise mcp authoring workflow should execute");
    let revise_result = revise_outcome.result.expect("revise workflow result");
    assert!(revise_result.contains("\"status\":\"promoted\""));
    let reuse_promotion_prompt = reuse_promotion_prompt
        .lock()
        .expect("reuse promotion prompt");
    assert!(
        reuse_promotion_prompt.contains(
            r#"{"app_id":"aaaa1111","workflow_run_id":"wf_mcp_authoring2"}"#
        ) && !reuse_promotion_prompt.contains("receipt_id"),
        "an approval-reuse promotion must omit the absent optional receipt: {reuse_promotion_prompt}"
    );

    let zero_tool_outcome = run_with_host(
        &source,
        move |_prompts, options| {
            options
                .iter()
                .map(|options| {
                    if options.contains("app-evidence") {
                        evidence_for_zero_tools.to_string()
                    } else if options.contains("host-proposal-validation") {
                        serde_json::json!({
                            "ok": true,
                            "status": "approval_required",
                            "proposal_sha256": "6".repeat(64),
                            "approval_contract_sha256": "7".repeat(64),
                            "tool_surface_sha256": "8".repeat(64),
                            "findings": []
                        })
                        .to_string()
                    } else {
                        serde_json::json!({
                            "appId": "aaaa1111",
                            "manifestRevision": 3,
                            "userGoalSha256": "0000000000000000000000000000000000000000000000000000000000000000",
                            "summary": "No bounded MCP surface available",
                            "tools": [],
                            "requiredFlowChanges": [],
                            "excludedCapabilities": []
                        })
                        .to_string()
                    }
                })
                .collect()
        },
        |_progress| {},
        None,
        false,
        Some(initial_args.to_string()),
        None,
    )
    .expect("zero-tool mcp authoring workflow should execute");
    let zero_tool_result = zero_tool_outcome.result.expect("zero-tool workflow result");
    assert!(zero_tool_result.contains("\"status\":\"mcp_authoring_required\""));
    assert!(zero_tool_result.contains("\"candidate_preserved\":true"));
    assert!(zero_tool_result.contains("\"validation\""));
}

#[test]
fn use_test_reads_persisted_authoring_spec_and_rejects_overrides() {
    let source = std::fs::read_to_string(workflow_dir().join("local-app-use-test.js"))
        .expect("read use-test workflow");
    let args = use_test_args("balanced");
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let calls_for_run = Arc::clone(&calls);
    let outcome = run_with_host(
        &source,
        move |_prompts, options| {
            options
                .iter()
                .map(|options| {
                    let label = stage_label(options);
                    calls_for_run.lock().expect("call log").push(label.clone());
                    passing_qa_reply(&label, "wf_use_test", "balanced")
                })
                .collect()
        },
        |_progress| {},
        None,
        true,
        Some(args.to_string()),
        None,
    )
    .expect("use-test should consume the Host-persisted contract");
    let result = outcome.result.expect("use-test result");
    assert!(result.contains("\"ok\":true"), "{result}");
    assert_eq!(
        calls.lock().expect("call log").as_slice(),
        ["operator-0", "tester-0"]
    );

    let mut forged = args;
    forged
        .as_object_mut()
        .expect("args")
        .insert("authoring_spec".into(), authoring_spec());
    let error = run_with_host(
        &source,
        |_prompts, _options| panic!("override must fail before agent dispatch"),
        |_progress| {},
        None,
        true,
        Some(forged.to_string()),
        None,
    )
    .expect_err("use-test must reject caller-supplied AuthoringSpec");
    assert!(
        error
            .to_string()
            .contains("unknown field(s): authoring_spec"),
        "{error}"
    );
}

#[test]
fn use_test_qa_prompts_do_not_echo_host_schemas_or_catalog_capabilities() {
    let source = std::fs::read_to_string(workflow_dir().join("local-app-use-test.js"))
        .expect("read use-test workflow");
    let prompts_seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let prompts_for_run = Arc::clone(&prompts_seen);
    let mut args = use_test_args("balanced");
    let context = args["host_context"]
        .as_object_mut()
        .expect("host context object");
    context["schemas"]["schema_secret"] =
        serde_json::Value::String("schema_secret_sentinel".into());
    context.insert(
        "template_catalog".into(),
        serde_json::json!({"catalog_secret": "catalog_secret_sentinel"}),
    );
    context.insert(
        "selector_capability".into(),
        serde_json::Value::String("selector_secret_sentinel".into()),
    );
    run_with_host(
        &source,
        move |prompts, options| {
            prompts
                .iter()
                .zip(options.iter())
                .map(|(prompt, options)| {
                    prompts_for_run
                        .lock()
                        .expect("prompt capture")
                        .push(prompt.clone());
                    passing_qa_reply(&stage_label(options), "wf_use_test", "balanced")
                })
                .collect()
        },
        |_progress| {},
        None,
        true,
        Some(args.to_string()),
        None,
    )
    .expect("use-test should retain only its bounded QA context");
    for prompt in prompts_seen.lock().expect("prompts").iter() {
        for secret in [
            "selector_secret_sentinel",
            "catalog_secret_sentinel",
            "schema_secret_sentinel",
        ] {
            assert!(
                !prompt.contains(secret),
                "use-test prompt leaked {secret}: {prompt}"
            );
        }
        assert!(
            prompt.contains("primary-action"),
            "use-test prompt lost the persisted acceptance identity: {prompt}"
        );
    }
}

#[test]
fn use_test_carries_bounded_requested_intent_through_initial_and_resample_prompts() {
    let source = std::fs::read_to_string(workflow_dir().join("local-app-use-test.js"))
        .expect("read use-test workflow");
    let prompts_seen = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let prompts_for_run = Arc::clone(&prompts_seen);
    let mut args = use_test_args("balanced");
    args["scope"] = serde_json::Value::String("focus-on-distinct-user-journey".into());
    args["scenarios"] = serde_json::json!(["distinct-scenario-alpha", "distinct-scenario-beta"]);
    let outcome = run_with_host(
        &source,
        move |prompts, options| {
            prompts
                .iter()
                .zip(options.iter())
                .map(|(prompt, options)| {
                    let label = stage_label(options);
                    prompts_for_run
                        .lock()
                        .expect("prompt capture")
                        .push((label.clone(), prompt.clone()));
                    if label.starts_with("operator-") {
                        return operator_report(&qa_handle_for_label(&label)).to_string();
                    }
                    if label == "tester-0" {
                        return serde_json::json!({
                            "status": "evidence_resample_required",
                            "qa_handle": "qa_00000000000000000000000000000000",
                            "findings": [],
                            "summary": "capture was incomplete"
                        })
                        .to_string();
                    }
                    passing_qa_reply(&label, "wf_use_test", "balanced")
                })
                .collect()
        },
        |_progress| {},
        None,
        true,
        Some(args.to_string()),
        None,
    )
    .expect("use-test requested intent flow should execute");
    assert!(outcome
        .result
        .expect("use-test result")
        .contains("\"ok\":true"));

    let prompts = prompts_seen.lock().expect("prompts");
    for label in [
        "operator-0",
        "tester-0",
        "operator-resample",
        "tester-resample",
    ] {
        let prompt = prompts
            .iter()
            .find(|(seen_label, _)| seen_label == label)
            .map(|(_, prompt)| prompt)
            .unwrap_or_else(|| panic!("missing {label} prompt: {prompts:?}"));
        assert!(
            prompt.contains("focus-on-distinct-user-journey"),
            "{label} prompt lost requested scope: {prompt}"
        );
        assert!(
            prompt.contains("distinct-scenario-alpha") && prompt.contains("distinct-scenario-beta"),
            "{label} prompt lost requested scenarios: {prompt}"
        );
        assert!(
            prompt.contains("untrusted") && prompt.contains("Host"),
            "{label} prompt must subordinate requested intent to Host authority: {prompt}"
        );
    }
}

#[test]
fn use_test_returns_host_candidate_failure_instead_of_throwing() {
    let source = std::fs::read_to_string(workflow_dir().join("local-app-use-test.js"))
        .expect("read use-test workflow");
    let outcome = run_with_host(
        &source,
        move |_prompts, options| {
            options
                .iter()
                .map(|options| failing_qa_reply(&stage_label(options), "wf_use_test", "balanced"))
                .collect()
        },
        |_progress| {},
        None,
        true,
        Some(use_test_args("balanced").to_string()),
        None,
    )
    .expect("a Host verification failure is a structured workflow outcome");
    let result = outcome.result.expect("use-test result");
    assert!(result.contains("\"ok\":false"), "{result}");
    assert!(
        result.contains("\"status\":\"verification_failed\""),
        "{result}"
    );
    assert!(result.contains("receipt-qa-result-tester"), "{result}");
    assert!(result.contains("the save button does nothing"), "{result}");
}

#[test]
fn use_test_returns_infrastructure_failure_without_throwing_or_resampling() {
    let source = std::fs::read_to_string(workflow_dir().join("local-app-use-test.js"))
        .expect("read use-test workflow");
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let calls_for_run = Arc::clone(&calls);
    let outcome = run_with_host(
        &source,
        move |_prompts, options| {
            options
                .iter()
                .map(|options| {
                    let label = stage_label(options);
                    calls_for_run.lock().expect("call log").push(label.clone());
                    if label.starts_with("operator-") {
                        partial_operator_report("qa_00000000000000000000000000000000").to_string()
                    } else {
                        serde_json::json!({
                            "status": "infrastructure_failed",
                            "qa_handle": "qa_00000000000000000000000000000000",
                            "findings": [],
                            "summary": "Host QA backend unavailable"
                        })
                        .to_string()
                    }
                })
                .collect()
        },
        |_progress| {},
        None,
        true,
        Some(use_test_args("balanced").to_string()),
        None,
    )
    .expect("infrastructure failure is a structured workflow outcome");
    let result = outcome.result.expect("use-test result");
    assert!(result.contains("\"ok\":false"), "{result}");
    assert!(
        result.contains("\"status\":\"infrastructure_failed\""),
        "{result}"
    );
    assert!(result.contains("\"evidence_resamples\":0"), "{result}");
    assert!(
        result.contains("\"unverified_target_ids\":[\"ipad\"]")
            && result.contains("\"unverified_scenario_ids\":[\"ipad-layout\"]"),
        "partial Host verification scope was lost on use-test infrastructure failure: {result}"
    );
    assert_eq!(
        calls.lock().expect("call log").as_slice(),
        ["operator-0", "tester-0"]
    );
}

#[test]
fn use_test_exhausted_evidence_resample_preserves_partial_scope() {
    let source = std::fs::read_to_string(workflow_dir().join("local-app-use-test.js"))
        .expect("read use-test workflow");
    let outcome = run_with_host(
        &source,
        move |_prompts, options| {
            options
                .iter()
                .map(|options| {
                    let label = stage_label(options);
                    if label.starts_with("operator-") {
                        return partial_operator_report(&qa_handle_for_label(&label)).to_string();
                    }
                    if label == "tester-0" || label == "tester-resample" {
                        return serde_json::json!({
                            "status": "evidence_resample_required",
                            "qa_handle": if label == "tester-0" {
                                "qa_00000000000000000000000000000000"
                            } else {
                                "qa_11111111111111111111111111111111"
                            },
                            "findings": [],
                            "summary": "capture was incomplete"
                        })
                        .to_string();
                    }
                    passing_qa_reply(&label, "wf_use_test", "balanced")
                })
                .collect()
        },
        |_progress| {},
        None,
        true,
        Some(use_test_args("balanced").to_string()),
        None,
    )
    .expect("use-test exhausted evidence resample should remain structured");
    let result = outcome.result.expect("use-test result");
    assert!(
        result.contains("\"status\":\"verification_failed\""),
        "{result}"
    );
    assert!(result.contains("\"evidence_resamples\":1"), "{result}");
    assert!(
        result.contains("\"unverified_target_ids\":[\"ipad\"]")
            && result.contains("\"unverified_scenario_ids\":[\"ipad-layout\"]"),
        "partial Host verification scope was lost after use-test resample exhaustion: {result}"
    );
}

#[test]
fn use_test_verifier_disposition_preserves_partial_scope() {
    let source = std::fs::read_to_string(workflow_dir().join("local-app-use-test.js"))
        .expect("read use-test workflow");
    let outcome = run_with_host(
        &source,
        move |_prompts, options| {
            options
                .iter()
                .map(|options| {
                    let label = stage_label(options);
                    if label.starts_with("operator-") {
                        return partial_operator_report(&qa_handle_for_label(&label)).to_string();
                    }
                    if label == "verifier" || label == "verifier-resample" {
                        return serde_json::json!({
                            "status": "evidence_resample_required",
                            "qa_handle": if label == "verifier" {
                                "qa_00000000000000000000000000000000"
                            } else {
                                "qa_11111111111111111111111111111111"
                            },
                            "findings": [],
                            "summary": "verifier capture was incomplete"
                        })
                        .to_string();
                    }
                    passing_qa_reply(&label, "wf_use_test", "thorough")
                })
                .collect()
        },
        |_progress| {},
        None,
        true,
        Some(use_test_args("thorough").to_string()),
        None,
    )
    .expect("use-test verifier disposition should remain structured");
    let result = outcome.result.expect("use-test result");
    assert!(
        result.contains("\"status\":\"verification_failed\""),
        "{result}"
    );
    assert!(
        result.contains("\"unverified_target_ids\":[\"ipad\"]")
            && result.contains("\"unverified_scenario_ids\":[\"ipad-layout\"]"),
        "partial Host verification scope was lost on use-test verifier disposition: {result}"
    );
}

#[test]
fn verifier_and_tester_agent_grants_remain_read_only() {
    let agents = workflow_dir().parent().expect("plugin root").join("agents");
    let verifier = std::fs::read_to_string(agents.join("verifier.md")).expect("verifier");
    let tester = std::fs::read_to_string(agents.join("tester.md")).expect("tester");
    for (role, source) in [("verifier", &verifier), ("tester", &tester)] {
        let grants = source.split("---").nth(1).unwrap_or_default();
        for forbidden in [
            "LocalAppInspectUi",
            "LocalAppCaptureUi",
            "LocalAppActOnUi",
            "LocalAppMutateData",
            "LocalAppBuild",
            "LocalAppPromoteMcpCandidate",
            "Write",
            "Edit",
        ] {
            assert!(
                !grants
                    .lines()
                    .any(|line| line.trim() == format!("- {forbidden}")),
                "{role} must not grant {forbidden}: {grants}"
            );
        }
    }
}

#[test]
fn mcp_promotion_uses_a_dedicated_tools_only_agent() {
    let agents = workflow_dir().parent().expect("plugin root").join("agents");
    let promoter = std::fs::read_to_string(agents.join("mcp-promoter.md")).expect("mcp promoter");
    let grants = promoter.split("---").nth(1).unwrap_or_default();
    assert!(
        grants
            .lines()
            .any(|line| line.trim() == "- LocalAppPromoteMcpCandidate"),
        "promoter must have the Host promotion tool: {grants}"
    );
    for forbidden in [
        "Write",
        "Edit",
        "LocalAppBuild",
        "LocalAppMutateData",
        "LocalAppQaFinalize",
    ] {
        assert!(
            !grants
                .lines()
                .any(|line| line.trim() == format!("- {forbidden}")),
            "promoter must not grant {forbidden}: {grants}"
        );
    }
    let workflow = std::fs::read_to_string(workflow_dir().join("local-app-mcp-authoring.js"))
        .expect("mcp workflow");
    assert!(
        workflow.contains("agentType: 'mcp-promoter'"),
        "promotion must run as the dedicated promoter role"
    );
    assert!(
        !workflow.contains("agentType: 'verifier', label: 'mcp-promote'"),
        "verifier must not own MCP promotion"
    );
    let inventory =
        std::fs::read_to_string(local_app_builder_plugin::inventory_path()).expect("plugin inventory");
    assert!(
        inventory
            .lines()
            .any(|line| line.trim() == "agents/mcp-promoter.md"),
        "promoter must be registered in the compiled plugin inventory"
    );
}

#[test]
fn mcp_proposal_schema_uses_the_actual_host_dto_wire_names() {
    let path = workflow_dir()
        .parent()
        .expect("plugin root")
        .join("schemas/mcp-proposal.schema.json");
    let schema: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("read MCP proposal schema"))
            .expect("parse MCP proposal schema");
    let properties = schema["properties"]
        .as_object()
        .expect("proposal properties");
    for required in [
        "appId",
        "manifestRevision",
        "userGoalSha256",
        "tools",
        "requiredFlowChanges",
        "excludedCapabilities",
    ] {
        assert!(
            properties.contains_key(required),
            "missing Host DTO field {required}"
        );
    }
    for stale in [
        "app_id",
        "manifest_revision",
        "user_goal_sha256",
        "required_flow_changes",
        "excluded_capabilities",
    ] {
        assert!(
            !properties.contains_key(stale),
            "stale non-DTO field {stale}"
        );
    }
    assert_eq!(schema["additionalProperties"], false);
    assert!(
        schema["properties"]["tools"].get("minItems").is_none(),
        "the Host DTO and workflow both support a zero-tool candidate"
    );
    let tool = schema["$defs"]["tool"]["properties"]
        .as_object()
        .expect("tool properties");
    for field in [
        "inputSchema",
        "outputSchema",
        "semanticFlowId",
        "inputs",
        "result",
    ] {
        assert!(
            tool.contains_key(field),
            "missing AppMcpToolProposal field {field}"
        );
    }
    let variants = schema["$defs"]["flow_value_binding"]["oneOf"]
        .as_array()
        .expect("FlowValueBinding variants");
    for tag in ["literal", "tool_input", "step_output"] {
        assert!(
            variants
                .iter()
                .any(|variant| variant["properties"].get(tag).is_some()),
            "missing serde FlowValueBinding tag {tag}"
        );
    }
}

#[test]
fn missing_qa_begin_projection_fails_closed_in_use_test() {
    let mut malformed = operator_report("qa_00000000000000000000000000000000");
    malformed
        .as_object_mut()
        .expect("operator fixture object")
        .remove("verification_scope");
    let malformed = malformed.to_string();

    let use_test = std::fs::read_to_string(workflow_dir().join("local-app-use-test.js"))
        .expect("read use-test workflow");
    let use_test_error = run_with_host(
        &use_test,
        move |_prompts, options| options.iter().map(|_options| malformed.clone()).collect(),
        |_progress| {},
        None,
        true,
        Some(use_test_args("balanced").to_string()),
        None,
    )
    .expect_err("use-test must reject an operator response without Host scope");
    assert!(
        use_test_error
            .to_string()
            .contains("complete Host QA scope and ledger projection"),
        "{use_test_error}"
    );
}
