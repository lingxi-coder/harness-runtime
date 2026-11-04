use super::*;

fn text(value: &str) -> Message {
    Message {
        role: "user".into(),
        api_output_config: None,
        content: vec![ContentBlock::Text {
            text: value.into(),
            citations: None,
            cache_control: None,
        }],
    }
}
fn project(
    cache: &mut MessageProjectionCache,
    messages: &[Message],
    family: wire::ProtocolFamily,
) -> wire::ChatRequest {
    history_input_with_cached_sources("m", messages, &[], &[], &[], family, Some(cache))
        .unwrap()
        .0
}
#[test]
fn cached_history_matches_fresh_projection_after_append_edit_compact_and_protocol_change() {
    let mut cache = MessageProjectionCache::default();
    let mut messages = vec![text("a"), text("b")];
    let family = wire::ProtocolFamily::AnthropicMessages;
    project(&mut cache, &messages, family);
    messages.push(text("c"));
    project(&mut cache, &messages, family);
    assert_eq!(cache.hits, 2);
    assert_eq!(cache.misses, 3);
    for change in ["edit", "compact", "protocol"] {
        let family = if change == "protocol" {
            wire::ProtocolFamily::OpenAiResponses
        } else {
            family
        };
        match change {
            "edit" => messages[1] = text("edited"),
            "compact" => messages = vec![text("summary")],
            _ => {}
        }
        let cached = project(&mut cache, &messages, family);
        let fresh = history_input("m", &messages, &[], &[], family).unwrap().0;
        assert_eq!(
            serde_json::to_string(&cached).unwrap(),
            serde_json::to_string(&fresh).unwrap(),
            "{change}"
        );
    }
    assert_eq!(cache.entries.len(), 1);
}
#[test]
fn cache_preserves_exact_utf16_replay_and_never_reuses_changed_same_length_content() {
    let family = wire::ProtocolFamily::AnthropicMessages;
    let payload =
        wire::JsonPayload::parse(r#"{"type":"text","text":"\ud800","integer":9007199254740993}"#)
            .unwrap();
    let mut messages = vec![Message {
        role: "assistant".into(),
        api_output_config: None,
        content: vec![ContentBlock::ProviderContent {
            protocol: "anthropic_messages".into(),
            value: payload,
        }],
    }];
    let mut cache = MessageProjectionCache::default();
    let first = project(&mut cache, &messages, family);
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&project(&mut cache, &messages, family)).unwrap()
    );
    assert_eq!(cache.hits, 1);
    messages[0] = text("changed");
    let changed = project(&mut cache, &messages, family);
    assert_ne!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&changed).unwrap()
    );
    assert_eq!(cache.misses, 2);
}
#[test]
fn projection_cache_is_bounded_and_releases_truncated_history() {
    let mut cache = MessageProjectionCache::default();
    let family = wire::ProtocolFamily::OpenAiResponses;
    project(
        &mut cache,
        &[text(&"x".repeat(MessageProjectionCache::MAX_BYTES))],
        family,
    );
    assert!(cache.entries.is_empty());
    project(&mut cache, &[text("small")], family);
    assert_eq!(cache.entries.len(), 1);
    project(&mut cache, &[], family);
    assert_eq!(cache.bytes, 0);
    assert!(cache.entries.is_empty());
}
#[test]
#[ignore = "local performance probe; run serially with --nocapture"]
fn history_projection_cost_probe() {
    use std::{hint::black_box, time::Instant};
    let messages: Vec<_> = (0..128)
        .map(|i| Message {
            role: "user".into(),
            api_output_config: None,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: format!("call-{i}"),
                output: wire::JsonPayload::parse(&format!(
                    "{{\"body\":\"{}\"}}",
                    "x".repeat(16 * 1024)
                ))
                .unwrap(),
                is_error: Some(false),
                cache_control: None,
                cache_reference: None,
            }],
        })
        .collect();
    let family = wire::ProtocolFamily::AnthropicMessages;
    let started = Instant::now();
    for _ in 0..8 {
        black_box(history_input("m", &messages, &[], &[], family).unwrap());
    }
    let fresh = started.elapsed();
    let mut cache = MessageProjectionCache::default();
    project(&mut cache, &messages, family);
    let started = Instant::now();
    for _ in 0..8 {
        black_box(project(&mut cache, &messages, family));
    }
    println!("history_messages={} tool_payload_bytes={} repetitions=8 fresh_us={} cached_us={} hits={} retained_bytes={}", messages.len(), 128*16*1024, fresh.as_micros(), started.elapsed().as_micros(), cache.hits, cache.bytes);
}
