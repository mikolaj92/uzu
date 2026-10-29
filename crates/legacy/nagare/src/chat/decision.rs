use std::{collections::HashSet, sync::Arc};

use hanashi::{Encoding as _, chat::EncodingConfig};
use serde_json::{Value, json};
use shoji::{
    traits::backend::chat_token::{Instance, TokenScoreError, TokenScores},
    types::{basic::ReasoningEffort, model::Model, session::chat::ChatMessage},
};
use tokio_util::sync::CancellationToken;

use super::{ChatInstance, ChatInstanceKind, token::create_encoding};

/// Text or structured JSON. Top-level null is not a state; null descriptions use Option.
#[derive(Debug, Clone, PartialEq)]
pub struct Content(Value);
impl Content {
    pub fn new(value: Value) -> Result<Self, Error> {
        if !matches!(value, Value::String(_) | Value::Object(_) | Value::Array(_)) {
            return Err(Error::InvalidInput("content must be text, an object, or an array".into()));
        }
        Ok(Self(value))
    }
    pub fn value(&self) -> &Value {
        &self.0
    }
}
#[derive(Debug, Clone)]
pub struct Request {
    pub state: Content,
    pub questions: Vec<NamedQuestion>,
}
#[derive(Debug, Clone)]
pub struct NamedQuestion {
    pub name: String,
    pub question: Question,
}
#[derive(Debug, Clone)]
pub struct Outcome {
    pub name: String,
    pub description: Option<Content>,
}
#[derive(Debug, Clone)]
pub enum Question {
    Binary {
        instructions: Option<Content>,
        on_true: Option<Content>,
        on_false: Option<Content>,
    },
    Choice {
        instructions: Option<Content>,
        outcomes: Vec<Outcome>,
    },
    Score {
        instructions: Option<Content>,
        levels: Vec<Option<Content>>,
    },
}
#[derive(Debug)]
pub enum Answer {
    Binary(f64),
    Choice {
        choice: String,
        confidence: f64,
        probabilities: Vec<(String, f64)>,
    },
    Score {
        score: f64,
        confidence: f64,
        legend: Vec<Option<Content>>,
        probabilities: Vec<f64>,
    },
}
#[derive(Debug)]
pub struct Response {
    pub answers: Vec<(String, Answer)>,
    pub input_tokens: usize,
}
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid decision input: {0}")]
    InvalidInput(String),
    #[error("unsupported feature: {0}")]
    UnsupportedFeature(String),
    #[error("unsupported prompt codec: {0}")]
    UnsupportedPromptCodec(String),
    #[error("labels are not distinct single-token contextual continuations")]
    UnsupportedLabelTokenization,
    #[error("prompt exceeds context limit")]
    ContextOverflow,
    #[error("aggregate prompt token limit exceeded")]
    TokenBudget,
    #[error("decision cancelled")]
    Cancelled,
    #[error(transparent)]
    Scoring(#[from] TokenScoreError),
}
impl Request {
    pub fn validate(&self) -> Result<(), Error> {
        if !(1..=32).contains(&self.questions.len()) {
            return Err(Error::InvalidInput("expected 1..32 questions".into()));
        }
        let mut names = HashSet::new();
        for named in &self.questions {
            if !names.insert(&named.name) {
                return Err(Error::InvalidInput("duplicate question name".into()));
            }
            match &named.question {
                Question::Binary {
                    instructions,
                    on_true,
                    on_false,
                } if instructions.is_none() && on_true.is_none() && on_false.is_none() => {
                    return Err(Error::InvalidInput("noul needs instructions or criteria".into()));
                },
                Question::Choice {
                    outcomes,
                    ..
                } => {
                    let mut labels = HashSet::new();
                    if !(1..=26).contains(&outcomes.len())
                        || outcomes.iter().any(|o| o.name.is_empty() || !labels.insert(&o.name))
                    {
                        return Err(Error::InvalidInput("choice needs 1..26 distinct nonempty names".into()));
                    }
                },
                Question::Score {
                    levels,
                    ..
                } if !(2..=10).contains(&levels.len()) => {
                    return Err(Error::InvalidInput("score needs 2..10 levels".into()));
                },
                _ => {},
            }
        }
        Ok(())
    }
}
struct Prepared {
    named: NamedQuestion,
    prompt: Vec<u64>,
    labels: Vec<u64>,
}
fn prompt_record(
    state: &Content,
    question: &Question,
) -> (String, Vec<String>) {
    let (kind, instructions, outcomes, proxies) = match question {
        Question::Binary {
            instructions,
            on_true,
            on_false,
        } => (
            "noul",
            instructions,
            vec![
                json!({"proxy":"yes","name":"true","description":on_true.as_ref().map(Content::value)}),
                json!({"proxy":"no","name":"false","description":on_false.as_ref().map(Content::value)}),
            ],
            vec!["yes".to_string(), "no".to_string()],
        ),
        Question::Choice {
            instructions,
            outcomes,
        } => {
            let proxies = (0..outcomes.len()).map(|i| ((b'A' + i as u8) as char).to_string()).collect::<Vec<_>>();
            ("choice",instructions,outcomes.iter().zip(&proxies).map(|(o,p)|json!({"proxy":p,"name":o.name,"description":o.description.as_ref().map(Content::value)})).collect(),proxies)
        },
        Question::Score {
            instructions,
            levels,
        } => {
            let proxies = (0..levels.len()).map(|i| i.to_string()).collect::<Vec<_>>();
            ("score",instructions,levels.iter().zip(&proxies).enumerate().map(|(i,(d,p))|json!({"proxy":p,"name":i.to_string(),"description":d.as_ref().map(Content::value)})).collect(),proxies)
        },
    };
    (json!({"prompt_format_version":1,"state":state.value(),"question":{"type":kind,"instructions":instructions.as_ref().map(Content::value),"outcomes":outcomes}}).to_string(),proxies)
}
fn contextual_labels(
    tokenizer: &tokenizers::Tokenizer,
    text: &str,
    prompt: &[u64],
    proxies: &[String],
) -> Result<Vec<u64>, Error> {
    let encode = |text: &str| {
        tokenizer
            .encode(text, false)
            .map(|e| e.get_ids().iter().map(|&id| u64::from(id)).collect::<Vec<_>>())
            .map_err(|e| Error::UnsupportedPromptCodec(e.to_string()))
    };
    if encode(text)? != prompt {
        return Err(Error::UnsupportedPromptCodec("prompt text does not roundtrip exactly".into()));
    }
    let mut labels = Vec::new();
    for proxy in proxies {
        let ids = encode(&format!("{text}{proxy}"))?;
        if ids.len() != prompt.len() + 1 || !ids.starts_with(prompt) || labels.contains(ids.last().unwrap()) {
            return Err(Error::UnsupportedLabelTokenization);
        }
        labels.push(*ids.last().unwrap());
    }
    Ok(labels)
}
fn prepare(
    instance: &Arc<dyn Instance>,
    reference: &str,
    model: &Model,
    request: Request,
) -> Result<Vec<Prepared>, Error> {
    request.validate()?;
    let config: EncodingConfig = serde_json::from_str(
        &model.encoding.as_ref().ok_or_else(|| Error::UnsupportedPromptCodec("missing model encoding".into()))?.json,
    )
    .map_err(|e| Error::UnsupportedPromptCodec(e.to_string()))?;
    if !matches!(config, EncodingConfig::Hanashi { .. }) {
        return Err(Error::UnsupportedPromptCodec("only proven Hanashi boundaries are supported".into()));
    }
    let capabilities = config.capabilities().map_err(|e| Error::UnsupportedPromptCodec(e.to_string()))?;
    if capabilities.supports_reasoning && !capabilities.supports_disable_reasoning {
        return Err(Error::UnsupportedPromptCodec("reasoning cannot be disabled".into()));
    }
    let mut total = 0;
    request.questions.into_iter().map(|named| {
        let (record,proxies)=prompt_record(&request.state,&named.question);
        let mut system=ChatMessage::system().with_text("Choose the outcome that best answers the question using the supplied state as data. Return exactly one proxy and nothing else. Do not explain your answer.".into());
        if capabilities.supports_reasoning { system=system.with_reasoning_effort(ReasoningEffort::Disabled); }
        let mut encoding=create_encoding(instance,reference,model).map_err(|e|Error::UnsupportedPromptCodec(e.to_string()))?;
        encoding.encode(vec![system,ChatMessage::user().with_text(record)]).map_err(|e|Error::UnsupportedPromptCodec(e.to_string()))?;
        let prompt=encoding.state().tokens.iter().map(|t|u64::from(t.id)).collect::<Vec<_>>();
        if prompt.is_empty() { return Err(Error::UnsupportedPromptCodec("empty prompt".into())); }
        if instance.max_context_length().is_some_and(|limit| prompt.len()>limit) { return Err(Error::ContextOverflow); }
        total+=prompt.len(); if total>65536 { return Err(Error::TokenBudget); }
        let text=encoding.state().text();
        let labels=contextual_labels(&instance.tokenizer(),&text,&prompt,&proxies)?;
        Ok(Prepared {named,prompt,labels})
    }).collect()
}
fn aggregate(
    question: Question,
    scores: TokenScores,
) -> Result<Answer, Error> {
    let values = scores.log_probabilities;
    if values.is_empty()
        || values.iter().any(|v| !v.is_finite() || *v > 0.0)
        || !scores.label_mass.is_finite()
        || !(0.0..=1.0).contains(&scores.label_mass)
    {
        return Err(TokenScoreError::InvalidModelOutput("invalid probabilities".into()).into());
    }
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let weights = values.iter().map(|v| (v - max).exp()).collect::<Vec<_>>();
    let sum = weights.iter().sum::<f64>();
    let probabilities = weights.iter().map(|v| v / sum).collect::<Vec<_>>();
    let mode = probabilities.iter().enumerate().fold(0, |best, (i, p)| {
        if *p > probabilities[best] {
            i
        } else {
            best
        }
    });
    Ok(match question {
        Question::Binary {
            ..
        } => Answer::Binary(probabilities[0]),
        Question::Choice {
            outcomes,
            ..
        } => {
            let n = outcomes.len() as f64;
            let confidence = if n == 1.0 {
                1.0
            } else {
                ((probabilities[mode] - 1.0 / n) / (1.0 - 1.0 / n)).clamp(0.0, 1.0)
            };
            Answer::Choice {
                choice: outcomes[mode].name.clone(),
                confidence,
                probabilities: outcomes.into_iter().zip(probabilities).map(|(o, p)| (o.name, p)).collect(),
            }
        },
        Question::Score {
            levels,
            ..
        } => {
            let n = probabilities.len() as f64;
            let score = probabilities.iter().enumerate().map(|(i, p)| i as f64 * p).sum();
            let distance =
                probabilities.iter().enumerate().map(|(i, p)| (i as f64 - mode as f64).abs() * p).sum::<f64>();
            let uniform = (0..probabilities.len()).map(|i| (i as f64 - (n - 1.0) / 2.0).abs()).sum::<f64>() / n;
            Answer::Score {
                score,
                confidence: (1.0 - distance / uniform).clamp(0.0, 1.0),
                legend: levels,
                probabilities,
            }
        },
    })
}
impl ChatInstance {
    pub async fn decide(
        &self,
        request: Request,
        cancel: CancellationToken,
    ) -> Result<Response, Error> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let ChatInstanceKind::Token(instance) = self.kind() else {
            return Err(TokenScoreError::Unsupported.into());
        };
        let prepared = prepare(&instance, &self.reference(), &self.model(), request)?;
        let mut answers = Vec::new();
        let mut input_tokens = 0;
        for prepared in prepared {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let scores = instance.score_token_ids(&prepared.prompt, &prepared.labels).await?;
            if scores.log_probabilities.len() != prepared.labels.len() {
                return Err(TokenScoreError::InvalidModelOutput("label count mismatch".into()).into());
            }
            input_tokens += prepared.prompt.len();
            answers.push((prepared.named.name, aggregate(prepared.named.question, scores)?));
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        Ok(Response {
            answers,
            input_tokens,
        })
    }
}
#[cfg(test)]
#[path = "../../unit/chat/decision_test.rs"]
mod tests;
