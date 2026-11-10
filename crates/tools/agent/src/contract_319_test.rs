use super::*;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../tests/fixtures/agent_contract_319.json")).unwrap()
}

fn completed(input: &Value, content: &[Vec<u16>]) -> Utf16JsonProjection {
    render_completed_model_content(
        content,
        input["agentId"].as_str().unwrap(),
        input["agentType"].as_str().unwrap_or(""),
        input["totalTokens"].as_u64().unwrap(),
        input["totalToolUseCount"].as_u64().unwrap(),
        input["totalDurationMs"].as_u64().unwrap(),
        input["worktreePath"]
            .as_str()
            .map(|path| (path, input["worktreeBranch"].as_str().unwrap_or(""))),
        input["canContinueAgent"].as_bool() != Some(false),
        None,
    )
}

#[test]
fn result_bytes_match_native_319_artifact() {
    let fixture = fixture();
    for case in fixture["completed"].as_array().unwrap() {
        let content: Vec<Vec<u16>> = case["input"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| block["text"].as_str().unwrap().encode_utf16().collect())
            .collect();
        let expected = case["output"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| block["text"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            completed(&case["input"], &content).string_at(""),
            Some(expected.as_str()),
            "{}",
            case["name"]
        );
    }
    for case in fixture["async"].as_array().unwrap() {
        let input = &case["input"];
        let actual = render_async_model_content(
            input["agentId"].as_str().unwrap(),
            input["outputFile"].as_str().unwrap(),
            input["canReadOutputFile"].as_bool() == Some(true),
            input["canContinueAgent"].as_bool() != Some(false),
        );
        assert_eq!(
            actual,
            case["output"]["content"][0]["text"].as_str().unwrap(),
            "{}",
            case["name"]
        );
    }
    for case in fixture["raw"].as_array().unwrap() {
        let content: Vec<Vec<u16>> =
            serde_json::from_value(case["input"]["contentUnits"].clone()).unwrap();
        let expected: Vec<u16> = serde_json::from_value(case["outputUnits"].clone()).unwrap();
        assert_eq!(
            completed(&case["input"], &content).string_units(""),
            Some(expected),
            "{}",
            case["name"]
        );
    }
}

#[test]
fn background_decisions_match_native_319_artifact() {
    for case in fixture()["background"].as_array().unwrap() {
        let input = &case["input"];
        let actual = should_run_in_background(BackgroundDecision {
            wants_background: input["wantsBackground"].as_bool(),
            definition_background: input["definitionBackground"].as_bool().unwrap(),
            is_builtin_web_fetch: input["isBuiltinWebFetch"].as_bool().unwrap(),
            is_coordinator: input["isCoordinator"].as_bool().unwrap(),
            force_async: input["forceAsync"].as_bool().unwrap(),
            caller_is_in_process_teammate: input["callerIsInProcessTeammate"].as_bool().unwrap(),
            caller_is_headless_subagent: input["callerIsHeadlessSubagent"].as_bool().unwrap(),
            background_tasks_disabled: input["backgroundTasksDisabled"].as_bool().unwrap(),
        });
        assert_eq!(actual, case["expected"].as_bool().unwrap(), "{input}");
    }
}

#[test]
fn effort_metadata_and_coercion_match_native_319_artifact() {
    let fixture = fixture();
    for case in fixture["effort"]["metadata"].as_array().unwrap() {
        let mut schema = AGENT_INPUT_SCHEMA_MODEL.clone();
        project_agent_effort_schema(
            &mut schema,
            case["fork"].as_bool().unwrap(),
            case["effortForced"].as_bool().unwrap(),
        );
        let mut expected = case["effort"].clone();
        if !expected.is_null() {
            expected.as_object_mut().unwrap().remove("optionalField");
            expected["description"] = Value::String(
                expected["description"]
                    .as_str()
                    .unwrap()
                    .replace("CLAUDE.md", "LINGXI.md"),
            );
        }
        assert_eq!(schema["properties"]["effort"], expected);
    }
    for case in fixture["effort"]["coercion"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["relativeEnabled"] == false)
    {
        let input = Utf16JsonProjection::from_serde_value(&case["input"]).unwrap();
        let actual = coerce_agent_effort_input(&input).map(|result| json!({
            "input": serde_json::from_str::<Value>(&result.input.to_json_string().unwrap()).unwrap(),
            "shapeClass": result.shape_class,
        })).unwrap_or(Value::Null);
        assert_eq!(actual, case["output"], "{}", case["input"]);
    }
}

#[test]
fn output_parser_retains_continuation_and_exact_task_call_input() {
    let source = Utf16JsonProjection::parse(r#"{"status":"completed","agentId":"a","prompt":"p","content":[],"totalTokens":0,"totalToolUseCount":0,"totalDurationMs":0,"usage":{"input_tokens":0,"output_tokens":0},"canContinueAgent":false,"taskCalls":[{"name":"TaskCreate","input":{"description":"\ud800"},"result":"created","extra":true}]}"#).unwrap();
    let parsed = agent_output::parse(&source, false).unwrap();
    assert_eq!(
        parsed
            .get_property("canContinueAgent")
            .and_then(|value| value.as_bool()),
        Some(false)
    );
    assert_eq!(
        parsed.string_units("/taskCalls/0/input/description"),
        Some(vec![0xd800])
    );
    assert!(parsed.at_pointer("/taskCalls/0/extra").is_none());
    let mut invalid = source.clone();
    invalid
        .set_field("canContinueAgent", source_text_projection("false"))
        .unwrap();
    assert!(agent_output::parse(&invalid, false).is_err());
    invalid = source;
    invalid
        .set_field(
            "taskCalls",
            generated_builtin_result(&json!([{"name":"Bash","input":{},"result":"done"}])),
        )
        .unwrap();
    assert!(agent_output::parse(&invalid, false).is_err());
}

#[test]
fn max_turn_notes_match_native_319_artifact() {
    for case in fixture()["maxTurnsNotes"].as_array().unwrap() {
        let limit = case["limit"].as_u64().unwrap();
        if limit == 0 {
            let result =
                generated_builtin_result(&json!({"reason":"max_turns_exhausted","max_turns":0}));
            assert!(max_turns_reached_from_result(&result).is_none());
            assert!(case["output"].is_null());
        } else {
            assert_eq!(
                max_turns_harness_note_with_continuation(
                    limit,
                    case["agentType"].as_str().unwrap(),
                    case["partial"].as_bool().unwrap(),
                    case["canContinue"].as_bool() != Some(false),
                ),
                case["output"].as_str().unwrap()
            );
        }
    }
}
