//! Native 2.1.287 per-query dispatch recovery. Transport stays in llm-client.

use std::time::Duration;

pub(crate) const DISPATCH_ID_HEADER: &str = "anthropic-dispatch-id";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DispatchHeaderState {
    pub auxiliary: bool,
    pub resending: bool,
    pub context_hint_beta: bool,
}

impl DispatchHeaderState {
    pub const AUXILIARY: Self = Self {
        auxiliary: true,
        resending: false,
        context_hint_beta: false,
    };

    pub fn header(self, first_party: bool, dreamy: bool, cedar: bool) -> Option<&'static str> {
        if !first_party {
            None
        } else if self.resending {
            Some("v2p")
        } else if dreamy {
            Some("v2d")
        } else if !self.auxiliary && cedar {
            Some("v2s")
        } else {
            None
        }
    }

    pub fn on_failure(
        &mut self,
        attempt: &DispatchAttempt,
        failure: DispatchFailure,
    ) -> Option<DispatchFallback> {
        if self.resending {
            return None;
        }
        let reason = if attempt.value.is_some() {
            if failure.status.is_some_and(|status| status >= 500) {
                "5xx"
            } else if failure.connection {
                "conn_err"
            } else {
                return None;
            }
        } else if attempt.first_party && failure.status == Some(503) {
            if failure.declined {
                "headerless_decline"
            } else {
                "headerless_retryable_503"
            }
        } else {
            return None;
        };
        self.resending = true;
        Some(DispatchFallback {
            previous: attempt.value.clone(),
            reason,
            status: failure.status,
            wait: attempt.value.is_none() && !failure.declined,
        })
    }

    pub fn on_body_failure(&mut self, attempt: &DispatchAttempt) -> Option<DispatchFallback> {
        if self.resending || attempt.value.is_none() {
            return None;
        }
        self.resending = true;
        Some(DispatchFallback {
            previous: attempt.value.clone(),
            reason: "body_phase",
            status: None,
            wait: false,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DispatchAttempt {
    pub value: Option<String>,
    pub first_party: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DispatchFailure {
    pub status: Option<u16>,
    pub connection: bool,
    pub declined: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct DispatchFallback {
    pub previous: Option<String>,
    pub reason: &'static str,
    pub status: Option<u16>,
    pub wait: bool,
}

impl DispatchFallback {
    /// Native htr -> sL(1): 500ms plus proportional jitter, rounded to ms.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn delay(&self, random: f64) -> Duration {
        if self.wait {
            Duration::from_millis((500.0 + random * 125.0).round() as u64)
        } else {
            Duration::ZERO
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../tests/fixtures/dispatch_recovery_2_1_287.json"
        ))
        .unwrap()
    }

    #[test]
    fn native_header_and_recovery_oracle() {
        for case in fixture()["cases"].as_array().unwrap() {
            let input = &case["input"];
            let expected = &case["expected"];
            let mut state = DispatchHeaderState {
                auxiliary: input["auxiliary"].as_bool().unwrap(),
                ..Default::default()
            };
            let first_party = input["firstParty"].as_bool().unwrap();
            let dreamy = input["dreamy"].as_bool().unwrap();
            let cedar = input["cedar"].as_bool().unwrap();
            let value = state.header(first_party, dreamy, cedar);
            assert_eq!(
                serde_json::to_value(value).unwrap(),
                expected["first"],
                "{input}"
            );
            let attempt = DispatchAttempt {
                value: value.map(str::to_owned),
                first_party,
            };
            let failure = DispatchFailure {
                status: input["status"]
                    .as_u64()
                    .map(|status| u16::try_from(status).unwrap()),
                connection: input["kind"] == "connection" || input["kind"] == "timeout",
                declined: input["declined"].as_bool().unwrap(),
            };
            let fallback = state.on_failure(&attempt, failure);
            assert_eq!(
                fallback.is_some(),
                expected["recovered"].as_bool().unwrap(),
                "{input}"
            );
            assert_eq!(
                state.resending,
                expected["resending"].as_bool().unwrap(),
                "{input}"
            );
            let retry = state.header(first_party, dreamy, cedar);
            assert_eq!(
                serde_json::to_value(retry).unwrap(),
                expected["retry"],
                "{input}"
            );
            if let Some(fallback) = fallback {
                let event = &expected["trace"][0]["metadata"];
                assert_eq!(
                    fallback.previous.as_deref().unwrap_or("none"),
                    event["dispatch"]
                );
                assert_eq!(fallback.reason, event["reason"]);
                assert_eq!(event["resend_dispatch"], "v2p");
                let status = fallback
                    .status
                    .map_or_else(|| Value::from("none"), Value::from);
                assert_eq!(status, event["status"]);
                let sleep = expected["trace"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find_map(|row| row["sleepMs"].as_u64())
                    .unwrap_or(0);
                assert_eq!(fallback.delay(0.5), Duration::from_millis(sleep));
            }
            let repeat = DispatchAttempt {
                value: retry.map(str::to_owned),
                first_party,
            };
            assert!(state.on_failure(&repeat, failure).is_none(), "{input}");
            assert_eq!(
                serde_json::to_value(state.header(first_party, dreamy, cedar)).unwrap(),
                expected["again"]
            );
        }
    }

    #[test]
    fn native_body_recovery_oracle() {
        for case in fixture()["bodyCases"].as_array().unwrap() {
            let input = &case["input"];
            let mut state = DispatchHeaderState {
                resending: input["resending"].as_bool().unwrap(),
                ..Default::default()
            };
            let attempt = DispatchAttempt {
                value: input["carried"].as_bool().unwrap().then(|| "v2d".into()),
                first_party: true,
            };
            let eligible =
                input["connection"].as_bool().unwrap() && !input["anyEvent"].as_bool().unwrap();
            let fallback = eligible.then(|| state.on_body_failure(&attempt)).flatten();
            assert_eq!(
                fallback.is_some(),
                case["expected"].as_bool().unwrap(),
                "{input}"
            );
            if let Some(fallback) = fallback {
                assert_eq!(fallback.reason, "body_phase");
                assert_eq!(fallback.previous.as_deref(), Some("v2d"));
                assert_eq!(state.header(true, false, false), Some("v2p"));
            }
        }
    }

    #[test]
    fn native_headerless_delay_oracle() {
        let fallback = DispatchFallback {
            previous: None,
            reason: "headerless_retryable_503",
            status: Some(503),
            wait: true,
        };
        for case in fixture()["delayCases"].as_array().unwrap() {
            assert_eq!(
                fallback.delay(case["random"].as_f64().unwrap()),
                Duration::from_millis(case["expected"].as_u64().unwrap())
            );
        }
    }
}
