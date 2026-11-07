//! Native remote decision adapters. Scheduling and settings belong to the host.
use async_trait::async_trait;
use decision_api::*;
use lingxi_llm_client::{
    protocol::Secret,
    providers::{
        openai::decisions::{CreateRequest, Decisions},
        typesafe::{SystemOne, SystemOneRequest},
    },
    transport::Transport,
    RequestOptions,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc, time::Instant};

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    OpenaiDecisions,
    TypesafeSystemone,
}

pub struct RemoteDecisionService {
    pub transport: Arc<dyn Transport>,
    pub protocol: Protocol,
    /// Complete resource URL, not a Chat endpoint.
    pub endpoint: String,
    pub credential: Option<Secret<String>>,
}

impl RemoteDecisionService {
    pub fn new(
        transport: Arc<dyn Transport>,
        protocol: Protocol,
        endpoint: String,
        credential: Option<String>,
    ) -> Self {
        Self {
            transport,
            protocol,
            endpoint,
            credential: credential.map(Secret::new),
        }
    }
}

pub fn encode_questions(request: &Request, protocol: Protocol) -> Value {
    let questions: BTreeMap<String, Value> = request.questions.iter().map(|(name, question)| {
        let value = match (protocol, question) {
            (Protocol::OpenaiDecisions, Question::Binary { instructions }) => json!({"type":"predicate", "name":name, "instructions":instructions}),
            (Protocol::OpenaiDecisions, Question::Choice { instructions, options }) => json!({"type":"choice", "name":name, "instructions":instructions, "choices": options.iter().map(|(value, description)| json!({"value":value,"description":description})).collect::<Vec<_>>()}),
            (Protocol::OpenaiDecisions, Question::Score { instructions, levels }) => json!({"type":"score", "name":name, "instructions":instructions, "levels":levels.iter().map(|level| json!({"label":level,"description":level})).collect::<Vec<_>>()}),
            (Protocol::TypesafeSystemone, Question::Binary { instructions }) => json!({"type":"noul", "instructions":instructions}),
            (Protocol::TypesafeSystemone, Question::Choice { instructions, options }) => json!({"type":"choice", "instructions":instructions,"criteria":options}),
            (Protocol::TypesafeSystemone, Question::Score { instructions, levels }) => json!({"type":"score", "instructions":instructions,"criteria":levels}),
        }; (name.clone(),value)
    }).collect();
    match protocol {
        Protocol::OpenaiDecisions => Value::Array(questions.into_values().collect()),
        Protocol::TypesafeSystemone => json!(questions),
    }
}
pub fn openai_input(request: &Request) -> Value {
    let evidence = if request.state.is_string() {
        json!({"type":"input_text","text":request.state})
    } else if request.state.is_object() || request.state.is_array() {
        json!({"type":"input_json","data":request.state})
    } else {
        json!({"type":"input_text","text":request.state.to_string()})
    };
    if request.images.is_empty() && request.state.is_string() {
        return request.state.clone();
    }
    let mut content = vec![evidence];
    content.extend(
        request
            .images
            .iter()
            .map(|image| json!({"type":"input_image","image_url":image.data_url})),
    );
    json!([{"role":"user","content":content}])
}
pub fn typesafe_input(request: &Request) -> (Value, BTreeMap<String, Value>) {
    let mut assets = BTreeMap::new();
    let mut references = Vec::new();
    for image in &request.images {
        let reference = uuid::Uuid::new_v4().to_string();
        let (prefix, data) = image
            .data_url
            .split_once(',')
            .expect("validated image data URL");
        assets.insert(reference.clone(), json!({"type":"image","format":if prefix.contains("image/png") {"png"} else {"jpeg"},"data_base64":data}));
        references.push(reference);
    }
    let state = if references.is_empty() {
        request.state.clone()
    } else {
        json!({"evidence":request.state,"images":references})
    };
    (state, assets)
}

fn invalid() -> Error {
    Error::Invalid("provider returned an invalid decision result".into())
}
fn number(value: &Value) -> Result<f64, Error> {
    value.as_f64().ok_or_else(invalid)
}

fn choice_probabilities(
    value: Option<&Value>,
    protocol: Protocol,
) -> Result<Option<BTreeMap<String, f64>>, Error> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    match protocol {
        Protocol::TypesafeSystemone => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|_| invalid()),
        Protocol::OpenaiDecisions => {
            let mut distribution = BTreeMap::new();
            for row in value.as_array().ok_or_else(invalid)? {
                let key = row["value"].as_str().ok_or_else(invalid)?;
                if distribution
                    .insert(key.into(), number(&row["probability"])?)
                    .is_some()
                {
                    return Err(invalid());
                }
            }
            Ok(Some(distribution))
        }
    }
}
fn score_probabilities(
    value: Option<&Value>,
    protocol: Protocol,
) -> Result<Option<Vec<f64>>, Error> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    match protocol {
        Protocol::TypesafeSystemone => {
            let mut distribution = BTreeMap::new();
            for (key, value) in value.as_object().ok_or_else(invalid)? {
                let index = key.parse::<u64>().map_err(|_| invalid())?;
                if index.to_string() != *key || distribution.insert(index, number(value)?).is_some()
                {
                    return Err(invalid());
                }
            }
            if distribution
                .keys()
                .copied()
                .ne(0..distribution.len() as u64)
            {
                return Err(invalid());
            }
            Ok(Some(distribution.into_values().collect()))
        }
        Protocol::OpenaiDecisions => {
            let mut distribution = BTreeMap::new();
            for row in value.as_array().ok_or_else(invalid)? {
                let index = row["value"].as_u64().ok_or_else(invalid)?;
                if distribution
                    .insert(index, number(&row["probability"])?)
                    .is_some()
                {
                    return Err(invalid());
                }
            }
            if distribution
                .keys()
                .copied()
                .ne(0..distribution.len() as u64)
            {
                return Err(invalid());
            }
            Ok(Some(distribution.into_values().collect()))
        }
    }
}

pub fn decode_response(
    request: &Request,
    protocol: Protocol,
    raw: Value,
    elapsed_ms: u64,
) -> Result<Response, Error> {
    let entries: BTreeMap<String, Value> = match protocol {
        Protocol::OpenaiDecisions => {
            let rows = raw["answers"].as_array().ok_or_else(invalid)?;
            let mut entries = BTreeMap::new();
            for row in rows {
                let name = row["name"].as_str().ok_or_else(invalid)?;
                if entries.insert(name.into(), row.clone()).is_some() {
                    return Err(invalid());
                }
            }
            entries
        }
        Protocol::TypesafeSystemone => {
            serde_json::from_value(raw["answers"].clone()).map_err(|_| invalid())?
        }
    };
    if entries.keys().ne(request.questions.keys()) {
        return Err(invalid());
    }
    let mut answers = BTreeMap::new();
    for (name, question) in &request.questions {
        let row = &entries[name];
        let expected_type = match (protocol, question) {
            (Protocol::OpenaiDecisions, Question::Binary { .. }) => "predicate",
            (Protocol::TypesafeSystemone, Question::Binary { .. }) => "noul",
            (_, Question::Choice { .. }) => "choice",
            (_, Question::Score { .. }) => "score",
        };
        if row["type"] != expected_type && row["type"] != "refusal" {
            return Err(invalid());
        }

        let confidence = row
            .get("confidence")
            .filter(|value| !value.is_null())
            .map(number)
            .transpose()?;
        let answer = if row["type"] == "refusal" {
            Answer::Refused {
                reason: row["reason"].as_str().map(str::to_owned),
            }
        } else {
            match question {
                Question::Binary { .. } => Answer::Binary {
                    probability: number(
                        &row[match protocol {
                            Protocol::OpenaiDecisions => "probability",
                            Protocol::TypesafeSystemone => "noul",
                        }],
                    )?,
                },
                Question::Choice { .. } => Answer::Choice {
                    option: row["choice"].as_str().ok_or_else(invalid)?.into(),
                    probabilities: choice_probabilities(row.get("probabilities"), protocol)?,
                    confidence,
                },
                Question::Score { .. } => Answer::Score {
                    value: number(&row["score"])?,
                    probabilities: score_probabilities(row.get("probabilities"), protocol)?,
                    confidence,
                },
            }
        };
        answers.insert(name.clone(), answer);
    }
    let response = Response {
        state_revision: request.state_revision,
        answers,
        usage: Usage {
            input_tokens: raw["usage"]["input_tokens"].as_u64(),
            output_tokens: raw["usage"]["output_tokens"].as_u64(),
        },
        actual_model: raw["model"].as_str().map(str::to_owned),
        elapsed_ms,
    };
    response.validate_for(request)?;
    Ok(response)
}

#[async_trait]
impl DecisionModel for RemoteDecisionService {
    async fn evaluate(
        &self,
        request: Request,
        context: InvocationContext,
    ) -> Result<Response, Error> {
        request.validate()?;
        let started = Instant::now();
        let options = RequestOptions {
            credential: self.credential.clone(),
            total_timeout: Some(context.timeout),
            ..Default::default()
        };
        let questions = encode_questions(&request, self.protocol);
        let operation = async {
            let result = match self.protocol {
                Protocol::OpenaiDecisions => {
                    Decisions::new(self.transport.as_ref(), &self.endpoint)
                        .create(
                            &CreateRequest {
                                model: request.model.model.clone(),
                                input: openai_input(&request),
                                questions: questions.as_array().unwrap().clone(),
                            },
                            &options,
                        )
                        .await
                }
                Protocol::TypesafeSystemone => {
                    let (state, assets) = typesafe_input(&request);
                    SystemOne::new(self.transport.as_ref(), &self.endpoint)
                        .evaluate(
                            &SystemOneRequest {
                                model: request.model.model.clone(),
                                state,
                                assets,
                                questions: serde_json::from_value(questions)
                                    .map_err(|_| invalid())?,
                            },
                            &options,
                        )
                        .await
                }
            }
            .map_err(|error| match error {
                lingxi_llm_client::providers::openai::decisions::NativeDecisionError::Transport(
                    lingxi_llm_client::protocol::LlmError::TransportTimeout { .. },
                ) => Error::Timeout,
                other => Error::Backend(other.to_string()),
            })?;
            decode_response(
                &request,
                self.protocol,
                result,
                started.elapsed().as_millis() as u64,
            )
        };
        tokio::select! { biased;
            _ = context.cancellation.cancelled() => Err(Error::Cancelled),
            result = tokio::time::timeout(context.timeout, operation) => result.map_err(|_| Error::Timeout)?,
        }
    }
}
