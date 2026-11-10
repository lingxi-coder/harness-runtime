#[test]
fn native_322_catalog_name_sanitizer_matches_pinned_names() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/mcp_raw_names_2_1_322.json"
    ))
    .unwrap();
    for case in fixture["toolCases"].as_array().unwrap() {
        let units = case["nameUnits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|unit| unit.as_u64().unwrap() as u16)
            .collect::<Vec<_>>();
        let clean = sanitize_catalog_name_units(&units);
        assert_eq!(clean, case["sanitized"].as_str().unwrap());
        assert_eq!(
            crate::normalization::normalize_name_for_mcp(&clean),
            case["normalized"].as_str().unwrap()
        );
    }
}

#[tokio::test]
async fn catalog_names_are_sanitized_but_direct_wire_names_retain_exact_units() {
    for era in [McpProtocolEra::Legacy, McpProtocolEra::Modern] {
        let (connection, peer_tx, mut peer_rx) = paired_connection();
        let client = Arc::new(
            McpClient::new("srv", PathBuf::from("/tmp/work"), connection)
                .await
                .with_negotiated_protocol(McpNegotiatedProtocol {
                    era,
                    version: if era == McpProtocolEra::Modern {
                        "2026-07-28"
                    } else {
                        "2025-11-25"
                    }
                    .into(),
                }),
        );
        let listed = {
            let client = client.clone();
            tokio::spawn(async move { client.list_tools().await })
        };
        let request = tokio::time::timeout(std::time::Duration::from_secs(2), peer_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let request: serde_json::Value = serde_json::from_slice(&request).unwrap();
        assert_eq!(request["method"], "tools/list");
        // Match the known-valid modern list shape used by the ordinary
        // client and input-required registry fixtures, including cache fields.
        let list_result = if era == McpProtocolEra::Modern {
            r#"{"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{"name":"echo \udfff","inputSchema":{"type":"object"}}]}"#
        } else {
            r#"{"tools":[{"name":"echo \udfff","inputSchema":{"type":"object"}}]}"#
        };
        peer_tx
            .send(exact_name_response_frame(
                Utf16JsonProjection::from_serde_value(&request["id"]).unwrap(),
                list_result,
            ))
            .await
            .unwrap();
        let tools = listed.await.unwrap().unwrap();
        assert_eq!(tools[0].tool_name, "echo ");
        assert_eq!(tools[0].full_name, "mcp__srv__echo_");
        assert!(tools[0].wire_tool_name.is_none());
        for direct in [false, true] {
            let raw = source_string_units(vec![101, 99, 104, 111, 32, 0xdfff]);
            let expected = if direct {
                raw.string_units("").unwrap()
            } else {
                "echo ".encode_utf16().collect()
            };
            let task = {
                let client = client.clone();
                let full_name = tools[0].full_name.clone();
                tokio::spawn(async move {
                    if direct {
                        client
                            .call_tool_with_wire_name(&full_name, raw, source_object(), None, None)
                            .await
                    } else {
                        client.call_tool(&full_name, source_object()).await
                    }
                })
            };
            let frame = tokio::time::timeout(std::time::Duration::from_secs(2), peer_rx.recv())
                .await
                .unwrap()
                .unwrap();
            let source =
                Utf16JsonProjection::parse(std::str::from_utf8(&frame).unwrap().trim()).unwrap();
            assert_eq!(source.string_units("/params/name"), Some(expected));
            // tools/call has its own complete shape and does not carry the
            // tools/list cache fields (input_required_registry_test.rs).
            let call_result = if era == McpProtocolEra::Modern {
                r#"{"resultType":"complete","content":[],"isError":false}"#
            } else {
                r#"{"content":[],"isError":false}"#
            };
            peer_tx
                .send(exact_name_response_frame(
                    source.subprojection("/id").unwrap(),
                    call_result,
                ))
                .await
                .unwrap();
            task.await.unwrap().unwrap();
        }
    }
}

fn exact_name_response_frame(id: Utf16JsonProjection, result: &str) -> Bytes {
    let response = Utf16JsonProjection::object(vec![
        ("jsonrpc".into(), source_string("2.0")),
        ("id".into(), id),
        ("result".into(), Utf16JsonProjection::parse(result).unwrap()),
    ])
    .unwrap();
    Bytes::from(response.to_json_string().unwrap() + "\n")
}
