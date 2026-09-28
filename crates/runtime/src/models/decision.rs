//! Extension contract for typed decision models, including a future Jev backend.
//!
//! This module performs no network requests and registers no production backend.
//! The calling policy owns state-version checks, thresholds and authorization.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Account-scoped selection for a decision capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    /// Implementation registered by the host.
    pub backend: String,
    /// Host account/profile identity, never secret material.
    pub account: String,
    /// Provider model identifier.
    pub model: String,
}

/// One named question about a state snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    /// Evaluate a proposition.
    Binary {
        /// Proposition to evaluate.
        instructions: String,
    },
    /// Select an explicitly named alternative.
    Choice {
        /// Selection criterion.
        instructions: String,
        /// Stable alternative IDs and descriptions.
        options: BTreeMap<String, String>,
    },
    /// Score against consecutive rubric levels starting at zero.
    Score {
        /// Property being scored.
        instructions: String,
        /// Ordered rubric descriptions.
        levels: Vec<String>,
    },
}

/// Prediction statistics remain optional when a backend does not provide them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    /// Probability of a true proposition.
    Binary {
        /// Finite value from zero to one.
        probability: f64,
    },
    /// Selected alternative.
    Choice {
        /// ID drawn from the requested options.
        option: String,
        /// Complete alternative distribution, when available.
        probabilities: Option<BTreeMap<String, f64>>,
        /// Backend-supplied confidence, when available.
        confidence: Option<f64>,
    },
    /// Expected rubric score.
    Score {
        /// Finite score within the requested rubric.
        value: f64,
        /// One probability per rubric level, when available.
        probabilities: Option<Vec<f64>>,
        /// Backend-supplied confidence, when available.
        confidence: Option<f64>,
    },
}

/// Typed decision input, separate from any chat request format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Selected account and model.
    pub model: ModelRef,
    /// Revision of the supplied state, used to reject stale decisions.
    pub state_revision: u64,
    /// Input state supplied by the caller.
    pub state: serde_json::Value,
    /// Nonempty set of uniquely named questions.
    pub questions: BTreeMap<String, Question>,
}

/// Explicitly observed token usage; absent values do not mean zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Observed input tokens.
    pub input_tokens: Option<u64>,
    /// Observed output tokens.
    pub output_tokens: Option<u64>,
}

/// Typed results associated with the original state revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Evaluated state revision.
    pub state_revision: u64,
    /// Exactly one matching answer per request question.
    pub answers: BTreeMap<String, Answer>,
    /// Backend observations.
    pub usage: Usage,
}

/// Host execution limits for a decision request.
#[derive(Clone)]
pub struct InvocationContext {
    /// Maximum wall-clock duration granted by the calling policy.
    pub timeout: Duration,
    /// Cooperative cancellation shared with the owning execution.
    pub cancellation: CancellationToken,
}

/// Capability errors are independent of LLM protocol errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// No decision implementation is registered for the binding.
    #[error("decision model unavailable")]
    Unavailable,
    /// The request or response violates the typed contract.
    #[error("invalid decision contract: {0}")]
    Invalid(String),
    /// The caller cancelled this invocation.
    #[error("decision cancelled")]
    Cancelled,
    /// The deadline expired.
    #[error("decision deadline exceeded")]
    Timeout,
    /// Sanitized provider error with no raw body or credential material.
    #[error("decision backend error: {0}")]
    Backend(String),
}

/// Optional backend capability; implementations do not execute tools or grant permissions.
#[async_trait]
pub trait DecisionModel: Send + Sync {
    /// Evaluate one state snapshot. The caller validates the response before use.
    async fn evaluate(
        &self,
        request: Request,
        context: InvocationContext,
    ) -> Result<Response, Error>;
}

impl Request {
    /// Validate a decision request before handing it to a future backend.
    pub fn validate(&self) -> Result<(), Error> {
        if [&self.model.backend, &self.model.account, &self.model.model]
            .iter()
            .any(|s| s.trim().is_empty())
        {
            return Err(Error::Invalid("incomplete model binding".into()));
        }
        if self.questions.is_empty() || self.questions.keys().any(|name| name.trim().is_empty()) {
            return Err(Error::Invalid("questions must have nonempty names".into()));
        }
        for question in self.questions.values() {
            match question {
                Question::Choice { options, .. }
                    if options.is_empty() || options.keys().any(|id| id.trim().is_empty()) =>
                {
                    return Err(Error::Invalid(
                        "choice options must have nonempty IDs".into(),
                    ));
                }
                Question::Score { levels, .. } if levels.is_empty() => {
                    return Err(Error::Invalid("score rubric must not be empty".into()));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn probability(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn distribution(values: impl Iterator<Item = f64>) -> bool {
    let mut sum = 0.0;
    for value in values {
        if !probability(value) {
            return false;
        }
        sum += value;
    }
    (sum - 1.0).abs() <= 1e-6
}

impl Response {
    /// Check correspondence and numeric domains without inventing confidence.
    pub fn validate_for(&self, request: &Request) -> Result<(), Error> {
        request.validate()?;
        if self.state_revision != request.state_revision
            || self.answers.keys().ne(request.questions.keys())
        {
            return Err(Error::Invalid(
                "state revision or question IDs do not match".into(),
            ));
        }
        for (name, question) in &request.questions {
            let answer = &self.answers[name];
            let valid = match (question, answer) {
                (Question::Binary { .. }, Answer::Binary { probability: value }) => {
                    probability(*value)
                }
                (
                    Question::Choice { options, .. },
                    Answer::Choice {
                        option,
                        probabilities,
                        confidence,
                    },
                ) => {
                    options.contains_key(option)
                        && confidence.is_none_or(probability)
                        && probabilities.as_ref().is_none_or(|values| {
                            values.keys().eq(options.keys())
                                && distribution(values.values().copied())
                        })
                }
                (
                    Question::Score { levels, .. },
                    Answer::Score {
                        value,
                        probabilities,
                        confidence,
                    },
                ) => {
                    value.is_finite()
                        && *value >= 0.0
                        && *value <= (levels.len() - 1) as f64
                        && confidence.is_none_or(probability)
                        && probabilities.as_ref().is_none_or(|values| {
                            values.len() == levels.len() && distribution(values.iter().copied())
                        })
                }
                _ => false,
            };
            if !valid {
                return Err(Error::Invalid(format!(
                    "invalid answer for question {name}"
                )));
            }
        }
        Ok(())
    }
}
