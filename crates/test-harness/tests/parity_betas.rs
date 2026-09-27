//! Parity driver: per-provider × per-endpoint expected `anthropic-beta` header.

use orchestrator::model::betas::{assemble_beta_header, BetaContext, Endpoint, Provider};
use serde::Deserialize;
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
struct Fixture {
    matrix: Vec<Row>,
}

#[derive(Deserialize)]
struct Row {
    provider: String,
    endpoint: String,
    model: String,
    expected: String,
}

#[test]
fn beta_header_matrix_matches_claude_code() {
    let fx: Fixture = load_fixture("betas");
    for row in fx.matrix {
        let p = match row.provider.as_str() {
            "anthropic" => Provider::Anthropic,
            "vertex" => Provider::Vertex,
            "bedrock" => Provider::Bedrock,
            other => panic!("unknown provider in fixture: {other:?}"),
        };
        let e = match row.endpoint.as_str() {
            "messages_create" => Endpoint::MessagesCreate,
            "messages_create_stream" => Endpoint::MessagesCreateStream,
            "count_tokens" => Endpoint::CountTokens,
            other => panic!("unknown endpoint in fixture: {other:?}"),
        };
        let got = assemble_beta_header(p, e, &BetaContext::for_model(&row.model));
        assert_eq!(
            got, row.expected,
            "beta header drift for {:?}/{:?} model {:?}\n  got:      {got}\n  expected: {}",
            row.provider, row.endpoint, row.model, row.expected,
        );
    }
}
