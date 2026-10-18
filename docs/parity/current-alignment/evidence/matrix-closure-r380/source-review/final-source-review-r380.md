# r380 refusal final-path source review

## Result

The prior batched request-route finding is fixed in the current source. The selected request model/profile follows the unique physical response ID into the refusal catalog gate; the request model also feeds Ko=false error text and the terminal error helper. Synthetic responses fall back to the selected query route; Frozen Native 2.1.295 does not gate refusal reissue on a physical response ID. I found no remaining source conflict in the refusal paths reviewed.

## Frozen source and bounded receipts

The target is Native 2.1.295, SHA-256 `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`. The query source is `src_202924418.js`, binary offset `202924418`, length `196943`, SHA-256 `2612bbe89324a30f9c773abfc2cc83802fd0340cd6283094ea1ed0413ba21e9d`, byte-equal to the frozen binary slice. `source-bindings-r380.json` binds the Native `Wa` classifier and `Qa` retry constructor; `max-turn-source-bindings-r380.json` records the exact turn-count spans.

Native's retry constructor receives the query-selected model (`model:u`, local offset `88727`, absolute offset `203013145`). The refusal retry branch (local offset `175765`, absolute offset `203100183`) emits `transition:{reason:"refusal_retry",model:Se}` and continues with the existing guards. It does not require a physical response ID. Ordinary next-turn count increments are at local offsets `192500`/`195052`; the refusal reissue does not consume a turn.

The historical full target receipt is **12/13**, followed by the exact **1/1** retest for the sole test-only fallback-event expectation correction. Those older receipts remain historical and are superseded for current refusal coverage by `runtime/r380-orchestrator-current-alignment-refusal-full14.receipt.json`: **14/14** passed on 2,330 stable source fingerprints. The physical-route alias selector also passed an exact **1/1** run on the same fingerprint in `runtime/r380-orchestrator-refusal-physical-route-alias-exact.receipt.json`; it is included in the 14-test full target. The current source changes and route behavior are now covered by the latest receipts.

## Current source checks

- `turn_loop.rs` selects a unique `PhysicalStepResponse` by `visible.id` and passes its request `model` and `profile` into `process_visible_response`; if no unique physical match exists it supplies the already-selected query model/profile. This prevents a provider-reported response model from changing the model catalog decision while matching Native's query-route behavior without inventing a response-ID precondition.
- `visible_response.rs` uses those private route fields for `native_refusal_retry_route_for_request`, Ko=false API-error staging, and the terminal API-error helper. The classifier remains fenced to the Native model catalog and excludes server-fallback events.
- The current route-alias regression exercises a custom physical route whose provider response reports an Anthropic model. It expects one physical request, no safety retry, and the terminal error label from the selected route; it passed both in the exact 1/1 selector and as part of the full 14/14 target.
- Ko=true buffered tool uses are settled before dispatch, preserving the assistant response identity and avoiding physical calls for unstarted tools. Ko=false still dispatches its ordinary tools and follows up; the transcript-only API-error row remains staged before tool results and outside the next provider history. The server-fallback guard stays ahead of Native refusal handling.
- `RefusalReissue` consumes one turn-loop reissue token without relaxing ordinary max-turn checks. The current full 14/14 target includes the max_turns=1 reissue selector and the ordinary-next-turn cap selector.

No Cargo or Native command was run during this review. Current refusal source review and its current-fingerprint Runtime validation are clear.
