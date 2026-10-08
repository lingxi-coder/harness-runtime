//! Current native stream-recovery decisions; physical execution belongs to the host.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Cause {
    Denied,
    Overloaded,
    ServerError,
    TimedOut,
    Stalled,
    ConnectionLost,
    Truncated,
    Malformed,
    BadRequest,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Progress {
    Output,
    ThinkingOnly,
    PartialOutput,
    Started,
    Nothing,
}
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub cause: Cause,
    pub progress: Progress,
    pub stop_reason_received: bool,
    pub outlasted_non_streaming_timeout: bool,
}
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    pub max_retries: u32,
    pub max_overloaded: u32,
    pub has_fallback_model: bool,
    pub persistent: bool,
    pub background: bool,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub retries: u32,
    pub overloaded: u32,
    pub stalls: u32,
    pub truncations: u32,
    pub after_thinking_only: u32,
    pub tried_without_streaming: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Decision {
    Fail,
    KeepPartial,
    Retry,
    RetryWithoutStreaming,
    UseFallbackModel,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct Outcome {
    pub decision: Decision,
    pub counts: Counts,
}
fn outcome(decision: Decision, counts: Counts) -> Outcome {
    Outcome { decision, counts }
}
fn fallback(counts: Counts, policy: Policy, allow: bool) -> Outcome {
    if !allow || counts.tried_without_streaming && counts.retries >= policy.max_retries {
        return outcome(Decision::Fail, counts);
    }
    outcome(
        Decision::RetryWithoutStreaming,
        Counts {
            retries: counts.retries.saturating_add(1).min(policy.max_retries),
            tried_without_streaming: true,
            ..counts
        },
    )
}
fn retry(counts: Counts, policy: Policy, exhausted: Outcome) -> Outcome {
    if counts.retries < policy.max_retries {
        outcome(
            Decision::Retry,
            Counts {
                retries: counts.retries + 1,
                ..counts
            },
        )
    } else {
        exhausted
    }
}
fn limited(
    mut counts: Counts,
    policy: Policy,
    value: u32,
    limit: u32,
    field: fn(&mut Counts) -> &mut u32,
    exhausted: Outcome,
) -> Outcome {
    if value >= limit {
        return exhausted;
    }
    *field(&mut counts) = value + 1;
    retry(counts, policy, exhausted)
}
fn overload(counts: Counts, policy: Policy, allow: bool) -> Outcome {
    let counts = Counts {
        overloaded: counts.overloaded.saturating_add(1),
        ..counts
    };
    let fail = fallback(counts, policy, allow);
    if policy.background && !policy.persistent {
        return if policy.has_fallback_model {
            outcome(Decision::UseFallbackModel, counts)
        } else {
            fail
        };
    }
    if counts.overloaded >= policy.max_overloaded {
        return if policy.has_fallback_model {
            outcome(Decision::UseFallbackModel, counts)
        } else {
            fail
        };
    }
    if policy.persistent {
        outcome(Decision::Retry, counts)
    } else {
        retry(counts, policy, fail)
    }
}
/// `uWo`/`dFe`/`oH`/`nH`/`Xle`, including counter ownership on exhausted branches.
#[must_use]
pub fn decide(
    failure: Failure,
    counts: Counts,
    policy: Policy,
    allow_non_streaming: bool,
) -> Outcome {
    use Cause::*;
    use Decision::*;
    use Progress::*;
    let fail = outcome(Fail, counts);
    let keep = outcome(KeepPartial, counts);
    let non_stream = fallback(counts, policy, allow_non_streaming);
    if failure.cause == Denied {
        return fail;
    }
    if failure.progress == Output {
        return if matches!(failure.cause, BadRequest | Unknown) {
            fail
        } else {
            keep
        };
    }
    if failure.progress == ThinkingOnly {
        return match failure.cause {
            Stalled if failure.stop_reason_received => keep,
            Stalled => limited(counts, policy, counts.stalls, 1, |c| &mut c.stalls, keep),
            ConnectionLost | Truncated | Malformed if failure.stop_reason_received => keep,
            ConnectionLost | Truncated | Malformed => limited(
                counts,
                policy,
                counts.after_thinking_only,
                2,
                |c| &mut c.after_thinking_only,
                keep,
            ),
            ServerError | TimedOut if failure.stop_reason_received => fail,
            ServerError | TimedOut if policy.has_fallback_model && !policy.persistent => {
                outcome(UseFallbackModel, counts)
            }
            ServerError | TimedOut => limited(
                counts,
                policy,
                counts.after_thinking_only,
                if failure.cause == TimedOut { 1 } else { 2 },
                |c| &mut c.after_thinking_only,
                fail,
            ),
            Overloaded if failure.stop_reason_received => fail,
            Overloaded => overload(counts, policy, allow_non_streaming),
            Denied | BadRequest | Unknown => fail,
        };
    }
    match failure.cause {
        Overloaded if failure.progress == PartialOutput => fallback(
            Counts {
                overloaded: counts.overloaded.saturating_add(1),
                ..counts
            },
            policy,
            allow_non_streaming,
        ),
        Overloaded => overload(counts, policy, allow_non_streaming),
        Stalled if failure.progress == Nothing => limited(
            counts,
            policy,
            counts.stalls,
            1,
            |c| &mut c.stalls,
            non_stream,
        ),
        Truncated if !failure.stop_reason_received => limited(
            counts,
            policy,
            counts.truncations,
            1,
            |c| &mut c.truncations,
            non_stream,
        ),
        ConnectionLost if !failure.stop_reason_received => retry(counts, policy, non_stream),
        ServerError
            if failure.outlasted_non_streaming_timeout
                && !failure.stop_reason_received
                && failure.progress != PartialOutput =>
        {
            limited(
                counts,
                policy,
                counts.after_thinking_only,
                2,
                |c| &mut c.after_thinking_only,
                non_stream,
            )
        }
        _ => non_stream,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_native_decisions_and_counter_ownership() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/stream_recovery_2_1_288.json"
        ))
        .unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 12800);
        for row in cases {
            let result = decide(
                serde_json::from_value(row["failure"].clone()).unwrap(),
                serde_json::from_value(row["counts"].clone()).unwrap(),
                serde_json::from_value(row["policy"].clone()).unwrap(),
                row["allowNonStreaming"].as_bool().unwrap(),
            );
            assert_eq!(
                serde_json::to_value(result).unwrap(),
                row["expected"],
                "{row}"
            );
        }
    }
}
