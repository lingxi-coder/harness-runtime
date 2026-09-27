//! Drive [`EffectHandler`] impls through the canonical contract suite.
//!
//! The reference subject is the harness-local [`AckEffectHandler`]: a
//! minimal handler that acks every variant. Downstream crates (e.g. the
//! cli-demo's run loop) layer their own contract drivers on top.

use test_harness::contracts::effect_handler::{effect_handler_contract_tests, AckEffectHandler};

#[tokio::test]
async fn ack_effect_handler_passes_contract() {
    let h = AckEffectHandler;
    effect_handler_contract_tests(&h).await;
}
