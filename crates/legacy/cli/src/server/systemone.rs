use std::{fmt, io::Cursor, time::Duration};

use indexmap::IndexMap;
use nagare::chat::decision::{self, Answer, Content, NamedQuestion, Outcome, Question};
use rocket::{
    Data, Request, State,
    data::ByteUnit,
    http::{ContentType, Status},
    request::{FromRequest, Outcome as GuardOutcome},
    response::{Responder, Response},
};
use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Value, json};

use super::ServerState;

#[derive(Debug)]
enum Wire {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Wire>),
    Object(IndexMap<String, Wire>),
}
impl<'de> Deserialize<'de> for Wire {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct WireVisitor;
        impl<'de> Visitor<'de> for WireVisitor {
            type Value = Wire;
            fn expecting(
                &self,
                f: &mut fmt::Formatter,
            ) -> fmt::Result {
                f.write_str("JSON value")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Wire, E> {
                Ok(Wire::Null)
            }
            fn visit_bool<E: de::Error>(
                self,
                v: bool,
            ) -> Result<Wire, E> {
                Ok(Wire::Bool(v))
            }
            fn visit_i64<E: de::Error>(
                self,
                v: i64,
            ) -> Result<Wire, E> {
                Ok(Wire::Number(v.into()))
            }
            fn visit_u64<E: de::Error>(
                self,
                v: u64,
            ) -> Result<Wire, E> {
                Ok(Wire::Number(v.into()))
            }
            fn visit_f64<E: de::Error>(
                self,
                v: f64,
            ) -> Result<Wire, E> {
                serde_json::Number::from_f64(v).map(Wire::Number).ok_or_else(|| E::custom("invalid number"))
            }
            fn visit_str<E: de::Error>(
                self,
                v: &str,
            ) -> Result<Wire, E> {
                Ok(Wire::String(v.into()))
            }
            fn visit_string<E: de::Error>(
                self,
                v: String,
            ) -> Result<Wire, E> {
                Ok(Wire::String(v))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<Wire, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = a.next_element()? {
                    out.push(v)
                }
                Ok(Wire::Array(out))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<Wire, A::Error> {
                let mut out = IndexMap::new();
                while let Some(k) = a.next_key::<String>()? {
                    if out.contains_key(&k) {
                        return Err(de::Error::custom("duplicate_key"));
                    }
                    out.insert(k, a.next_value()?);
                }
                Ok(Wire::Object(out))
            }
        }
        deserializer.deserialize_any(WireVisitor)
    }
}
impl Wire {
    fn value(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(v) => json!(v),
            Self::Number(v) => Value::Number(v),
            Self::String(v) => json!(v),
            Self::Array(v) => Value::Array(v.into_iter().map(Wire::value).collect()),
            Self::Object(v) => Value::Object(v.into_iter().map(|(k, v)| (k, v.value())).collect()),
        }
    }
    fn object(self) -> Result<IndexMap<String, Wire>, Failure> {
        if let Self::Object(v) = self {
            Ok(v)
        } else {
            Err(invalid("expected object"))
        }
    }
}
#[derive(Debug)]
struct Failure {
    status: Status,
    code: &'static str,
    message: String,
}
fn failure(
    status: Status,
    code: &'static str,
    message: impl Into<String>,
) -> Failure {
    Failure {
        status,
        code,
        message: message.into(),
    }
}
fn invalid(message: impl Into<String>) -> Failure {
    failure(Status::BadRequest, "invalid_request", message)
}
fn content(value: Wire) -> Result<Content, Failure> {
    Content::new(value.value()).map_err(domain_error)
}
fn description(value: Option<Wire>) -> Result<Option<Content>, Failure> {
    match value {
        None | Some(Wire::Null) => Ok(None),
        Some(value) => content(value).map(Some),
    }
}
fn unknown(fields: IndexMap<String, Wire>) -> Result<(), Failure> {
    if let Some((name, _)) = fields.first() {
        let code = if matches!(name.as_str(), "stream" | "temperature" | "images" | "image" | "box") {
            "unsupported_feature"
        } else {
            "unknown_field"
        };
        return Err(failure(Status::BadRequest, code, format!("unsupported field: {name}")));
    }
    Ok(())
}
fn parse(
    body: &str,
    model: &str,
) -> Result<decision::Request, Failure> {
    let wire = serde_json::from_str::<Wire>(body).map_err(|e| {
        failure(
            Status::BadRequest,
            if e.to_string().contains("duplicate_key") {
                "duplicate_key"
            } else {
                "invalid_request"
            },
            "invalid JSON request",
        )
    })?;
    let mut root = wire.object()?;
    if let Some(value) = root.shift_remove("model") {
        match value {
            Wire::String(id) if id == model => {},
            Wire::String(_) => {
                return Err(failure(Status::BadRequest, "model_not_found", "model must match the loaded model ID"));
            },
            _ => return Err(invalid("model must be a string")),
        }
    }
    if let Some(assets) = root.shift_remove("assets")
        && !matches!(assets, Wire::Null)
        && !matches!(&assets, Wire::Object(values) if values.is_empty())
    {
        return Err(failure(Status::BadRequest, "unsupported_feature", "assets are not supported"));
    }
    let state = content(root.shift_remove("state").ok_or_else(|| invalid("state is required"))?)?;
    let questions = root.shift_remove("questions").ok_or_else(|| invalid("questions is required"))?.object()?;
    unknown(root)?;
    let questions = questions
        .into_iter()
        .map(|(name, value)| {
            let mut fields = value.object()?;
            let kind = match fields.shift_remove("type") {
                Some(Wire::String(kind)) => kind,
                _ => return Err(invalid("question type is required")),
            };
            let instructions = description(fields.shift_remove("instructions"))?;
            let criteria = fields.shift_remove("criteria");
            let question = match kind.as_str() {
                "noul" => {
                    let mut criteria = match criteria {
                        None | Some(Wire::Null) => IndexMap::new(),
                        Some(value) => value.object()?,
                    };
                    let on_true = description(criteria.shift_remove("true"))?;
                    let on_false = description(criteria.shift_remove("false"))?;
                    unknown(criteria)?;
                    Question::Binary {
                        instructions,
                        on_true,
                        on_false,
                    }
                },
                "choice" => {
                    let criteria = criteria.ok_or_else(|| invalid("choice criteria are required"))?.object()?;
                    let outcomes = criteria
                        .into_iter()
                        .map(|(name, value)| {
                            Ok(Outcome {
                                name,
                                description: description(Some(value))?,
                            })
                        })
                        .collect::<Result<Vec<_>, Failure>>()?;
                    Question::Choice {
                        instructions,
                        outcomes,
                    }
                },
                "score" => {
                    let Some(Wire::Array(levels)) = criteria else {
                        return Err(invalid("score criteria must be an array"));
                    };
                    Question::Score {
                        instructions,
                        levels: levels.into_iter().map(|v| description(Some(v))).collect::<Result<Vec<_>, _>>()?,
                    }
                },
                "box" => return Err(failure(Status::BadRequest, "unsupported_feature", "box is not supported")),
                _ => return Err(invalid("unknown question type")),
            };
            unknown(fields)?;
            Ok(NamedQuestion {
                name,
                question,
            })
        })
        .collect::<Result<Vec<_>, Failure>>()?;
    let request = decision::Request {
        state,
        questions,
    };
    request.validate().map_err(domain_error)?;
    Ok(request)
}
fn domain_error(error: decision::Error) -> Failure {
    use decision::Error as E;
    use shoji::traits::backend::chat_token::TokenScoreError as S;
    let (status, code) = match &error {
        E::InvalidInput(_) => (Status::BadRequest, "invalid_request"),
        E::UnsupportedFeature(_) => (Status::BadRequest, "unsupported_feature"),
        E::UnsupportedPromptCodec(_) => (Status::UnprocessableEntity, "unsupported_prompt_codec"),
        E::UnsupportedLabelTokenization => (Status::UnprocessableEntity, "unsupported_label_tokenization"),
        E::ContextOverflow | E::Scoring(S::ContextOverflow) => (Status::UnprocessableEntity, "context_length_exceeded"),
        E::TokenBudget => (Status::PayloadTooLarge, "work_budget_exceeded"),
        E::Cancelled => (Status::GatewayTimeout, "deadline_exceeded"),
        E::Scoring(S::Unsupported) => (Status::NotImplemented, "native_scoring_unavailable"),
        E::Scoring(S::InvalidModelOutput(_)) => (Status::InternalServerError, "invalid_model_output"),
        _ => (Status::InternalServerError, "inference_error"),
    };
    failure(status, code, code.replace('_', " "))
}
#[derive(serde::Serialize)]
#[serde(tag = "type")]
enum WireAnswer {
    #[serde(rename = "noul")]
    Binary {
        noul: f64,
    },
    #[serde(rename = "choice")]
    Choice {
        choice: String,
        confidence: f64,
        probabilities: IndexMap<String, f64>,
    },
    #[serde(rename = "score")]
    Score {
        score: f64,
        confidence: f64,
        legend: IndexMap<String, Option<Value>>,
        probabilities: IndexMap<String, f64>,
    },
}
fn response(
    model: &str,
    result: decision::Response,
) -> String {
    let answers = result
        .answers
        .into_iter()
        .map(|(name, answer)| {
            let answer = match answer {
                Answer::Binary(noul) => WireAnswer::Binary {
                    noul,
                },
                Answer::Choice {
                    choice,
                    confidence,
                    probabilities,
                } => WireAnswer::Choice {
                    choice,
                    confidence,
                    probabilities: probabilities.into_iter().collect(),
                },
                Answer::Score {
                    score,
                    confidence,
                    legend,
                    probabilities,
                } => WireAnswer::Score {
                    score,
                    confidence,
                    legend: legend
                        .into_iter()
                        .enumerate()
                        .map(|(i, v)| (i.to_string(), v.map(|c| c.value().clone())))
                        .collect(),
                    probabilities: probabilities.into_iter().enumerate().map(|(i, p)| (i.to_string(), p)).collect(),
                },
            };
            (name, answer)
        })
        .collect::<IndexMap<_, _>>();
    #[derive(serde::Serialize)]
    struct Body<'a> {
        model: &'a str,
        answers: IndexMap<String, WireAnswer>,
        usage: Value,
    }
    serde_json::to_string(&Body {
        model,
        answers,
        usage: json!({"input_tokens":result.input_tokens,"output_tokens":0}),
    })
    .expect("finite validated decision response")
}
pub struct SystemOneResponse {
    status: Status,
    body: String,
    retry: bool,
}
impl From<Failure> for SystemOneResponse {
    fn from(error: Failure) -> Self {
        Self{status:error.status,retry:error.status==Status::TooManyRequests,body:json!({"error":{"message":error.message,"type":if error.status.code>=500 || error.status==Status::TooManyRequests {"server_error"}else{"invalid_request_error"},"code":error.code,"param":null}}).to_string()}
    }
}
impl<'r> Responder<'r, 'static> for SystemOneResponse {
    fn respond_to(
        self,
        _: &'r Request<'_>,
    ) -> rocket::response::Result<'static> {
        let mut response = Response::build();
        response.status(self.status).header(ContentType::JSON);
        if self.retry {
            response.raw_header("Retry-After", "1");
        }
        response.sized_body(self.body.len(), Cursor::new(self.body)).ok()
    }
}
pub struct Transport {
    json: bool,
    compressed: bool,
    deadline: Option<tokio::time::Instant>,
}
#[rocket::async_trait]
impl<'r> FromRequest<'r> for Transport {
    type Error = std::convert::Infallible;
    async fn from_request(request: &'r Request<'_>) -> GuardOutcome<Self, Self::Error> {
        GuardOutcome::Success(Self {
            deadline: request.deadline().map(Into::into),
            json: request.content_type().is_some_and(|content_type| content_type.is_json()),
            compressed: request
                .headers()
                .get("Content-Encoding")
                .any(|value| !value.trim().is_empty() && !value.eq_ignore_ascii_case("identity")),
        })
    }
}
#[rocket::catch(504)]
pub fn deadline_exceeded() -> SystemOneResponse {
    failure(Status::GatewayTimeout, "deadline_exceeded", "request deadline exceeded").into()
}

#[rocket::catch(505)]
pub fn unsupported_http_version() -> SystemOneResponse {
    failure(Status::HttpVersionNotSupported, "unsupported_http_version", "SystemOne requires HTTP/1.x").into()
}

struct CancelOnDrop(shoji::types::basic::CancelToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
#[rocket::post("/systemone", data = "<body>")]
pub async fn handle_systemone(
    body: Data<'_>,
    state: &State<ServerState>,
    transport: Transport,
) -> SystemOneResponse {
    if transport.compressed {
        return failure(
            Status::UnsupportedMediaType,
            "unsupported_content_encoding",
            "compressed request bodies are not supported",
        )
        .into();
    }
    if !transport.json {
        return failure(Status::UnsupportedMediaType, "unsupported_media_type", "expected application/json").into();
    }
    let deadline = transport.deadline.unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(30));
    let body = match tokio::time::timeout_at(deadline, body.open(ByteUnit::Mebibyte(1)).into_string()).await {
        Ok(Ok(body)) if body.is_complete() => body.into_inner(),
        Ok(Ok(_)) => return failure(Status::PayloadTooLarge, "request_too_large", "body exceeds 1 MiB").into(),
        Ok(Err(_)) => return invalid("failed to read request body").into(),
        Err(_) => return failure(Status::GatewayTimeout, "deadline_exceeded", "request deadline exceeded").into(),
    };
    let request = match parse(&body, &state.model_name) {
        Ok(request) => request,
        Err(error) => return error.into(),
    };
    if tokio::time::Instant::now() >= deadline {
        return failure(Status::GatewayTimeout, "deadline_exceeded", "request deadline exceeded").into();
    }
    let permit = match state.inference_gate.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return failure(Status::TooManyRequests, "model_busy", "model is busy").into(),
    };
    let cancel = shoji::types::basic::CancelToken::new();
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    let instance = state.instance.clone();
    let runtime = tokio::runtime::Handle::current();
    let worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if tokio::time::Instant::now() >= deadline || cancel.is_cancelled() {
            return Err(decision::Error::Cancelled);
        }
        runtime.block_on(instance.decide(request, cancel.into_inner()))
    });
    let result = tokio::time::timeout_at(deadline, worker).await;
    if tokio::time::Instant::now() >= deadline {
        return failure(Status::GatewayTimeout, "deadline_exceeded", "request deadline exceeded").into();
    }
    match result {
        Ok(Ok(Ok(result))) => SystemOneResponse {
            status: Status::Ok,
            body: response(&state.model_name, result),
            retry: false,
        },
        Ok(Ok(Err(error))) => domain_error(error).into(),
        Ok(Err(_)) => failure(Status::InternalServerError, "inference_error", "decision worker failed").into(),
        Err(_) => failure(
            Status::GatewayTimeout,
            "deadline_exceeded",
            "request deadline exceeded; active scoring will finish before releasing the model",
        )
        .into(),
    }
}
#[cfg(test)]
#[path = "../../unit/server/systemone_test.rs"]
mod tests;
