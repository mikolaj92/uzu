use std::{
    pin::Pin,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use hanashi::chat::{EncodingConfig, hanashi::config::HanashiConfig};
use shoji::{
    traits::backend::{
        Error, Instance as BackendInstance, InstanceStream, State,
        chat_token::{Instance, StreamInput, StreamOutput, TokenScoreError, TokenScores, TokenStreamMetrics},
    },
    types::{
        basic::SamplingParameters,
        model::{ModelAccessibility, ModelSource},
        session::chat::ChatReplyConfig,
    },
};
use tokenizers::{AddedToken, Tokenizer, models::bpe::BPE, pre_tokenizers::byte_level::ByteLevel};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::chat::decision::{self, Content, NamedQuestion, Outcome, Question, Request};

struct Numeric {
    tokenizer: Arc<Tokenizer>,
    calls: AtomicUsize,
    prompts: Mutex<Vec<Vec<u64>>>,
    cancel: Option<CancellationToken>,
    limit: Option<usize>,
    supported: bool,
    malformed: Mutex<Option<Vec<f64>>>,
}
impl BackendInstance for Numeric {
    type StreamConfig = ChatReplyConfig;
    type StreamInput = StreamInput;
    type StreamOutput = StreamOutput;
    type StreamMetrics = Option<TokenStreamMetrics>;
    fn state(&self) -> Pin<Box<dyn Future<Output = Result<Box<dyn State>, Error>> + Send + '_>> {
        panic!("decisions must not allocate generation state")
    }
    fn stream<'a>(
        &'a self,
        _: &'a StreamInput,
        _: &'a mut dyn State,
        _: ChatReplyConfig,
        _: CancellationToken,
    ) -> Pin<Box<dyn InstanceStream<Item = Result<StreamOutput, Error>, Metrics = Self::StreamMetrics> + Send + 'a>>
    {
        panic!("decisions must not generate")
    }
    fn peak_memory_usage(&self) -> Option<usize> {
        None
    }
}
impl Instance for Numeric {
    fn with_config(
        &self,
        _: ChatConfig,
    ) -> Box<dyn Instance> {
        panic!("decisions must not create a second instance")
    }
    fn tokenizer(&self) -> Arc<Tokenizer> {
        self.tokenizer.clone()
    }
    fn max_context_length(&self) -> Option<usize> {
        self.limit
    }
    fn stop_token_ids(&self) -> Option<Box<[u64]>> {
        None
    }
    fn sampling_defaults(&self) -> SamplingParameters {
        panic!("decisions must not sample")
    }
    fn score_token_ids<'a>(
        &'a self,
        prompt: &'a [u64],
        labels: &'a [u64],
    ) -> Pin<Box<dyn Future<Output = Result<TokenScores, TokenScoreError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.prompts.lock().unwrap().push(prompt.to_vec());
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
            if !self.supported {
                return Err(TokenScoreError::Unsupported);
            }
            Ok(TokenScores {
                log_probabilities: self
                    .malformed
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| vec![-10.0; labels.len()])
                    .into_boxed_slice(),
                label_mass: 0.001,
            })
        })
    }
}
fn fixture(
    cancel: Option<CancellationToken>,
    limit: Option<usize>,
    supported: bool,
) -> (ChatInstance, Arc<Numeric>) {
    let mut config = HanashiConfig::Qwen35.resolve().unwrap();
    let mut alphabet = ByteLevel::alphabet().into_iter().collect::<Vec<_>>();
    alphabet.sort_unstable();
    let vocab: [_; 256] = alphabet
        .into_iter()
        .enumerate()
        .map(|(i, c)| (c.to_string(), i as u32))
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let mut tokenizer = Tokenizer::new(BPE::builder().vocab_and_merges(vocab, Vec::new()).build().unwrap());
    tokenizer.with_pre_tokenizer(Some(ByteLevel::new(false, false, false)));
    tokenizer.with_decoder(Some(ByteLevel::new(false, false, false)));
    let mut markers = config.parsing.framing_config().tokens;
    markers.extend(["<|im_start|>".into(), "<|im_end|>".into(), "<think>".into(), "</think>".into()]);
    tokenizer.add_special_tokens(&markers.into_iter().map(|s| AddedToken::from(s, true)).collect::<Vec<_>>());
    tokenizer.add_tokens(&["yes", "no", "system", "user", "assistant"].map(|s| AddedToken::from(s, false)));
    config.tokens.bos_token_id = None;
    config.tokens.eos_token_id = tokenizer.token_to_id("<|im_end|>");
    let mut encoding = serde_json::to_value(EncodingConfig::Hanashi {
        config: HanashiConfig::Custom {
            config,
        },
    })
    .unwrap();
    encoding["rendering"]["jinja"]["required_functions"] = serde_json::json!([]);
    let model = Model::external(
        "test".into(),
        "test".into(),
        "Test".into(),
        "test".into(),
        "Test".into(),
        "1".into(),
        vec![],
        ModelAccessibility::OnDevice {
            source: ModelSource::Filesystem {
                path: String::new(),
            },
        },
        Some(encoding.into()),
    );
    let backend = Arc::new(Numeric {
        tokenizer: Arc::new(tokenizer),
        calls: AtomicUsize::new(0),
        prompts: Mutex::new(Vec::new()),
        cancel,
        limit,
        supported,
        malformed: Mutex::new(None),
    });
    (
        ChatInstance {
            kind: ChatInstanceKind::Token(backend.clone()),
            model,
            reference: String::new(),
        },
        backend,
    )
}
fn question(
    name: &str,
    instructions: &str,
) -> NamedQuestion {
    NamedQuestion {
        name: name.into(),
        question: Question::Choice {
            instructions: Some(Content::new(serde_json::json!(instructions)).unwrap()),
            outcomes: vec![Outcome {
                name: "only".into(),
                description: None,
            }],
        },
    }
}
fn request(questions: Vec<NamedQuestion>) -> Request {
    Request {
        state: Content::new(serde_json::json!({"type":"audio","status":"transcribed"})).unwrap(),
        questions,
    }
}
#[tokio::test]
async fn decisions_use_only_numeric_capability_fresh_prompts_and_actual_usage() {
    let (instance, backend) = fixture(None, None, true);
    let result = instance
        .decide(
            request(vec![question("z", "first unique instruction"), question("a", "second unique instruction")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    let prompts = backend.prompts.lock().unwrap();
    assert_eq!(result.input_tokens, prompts.iter().map(Vec::len).sum::<usize>());
    let decode =
        |p: &[u64]| backend.tokenizer.decode(&p.iter().map(|id| *id as u32).collect::<Vec<_>>(), false).unwrap();
    assert!(!decode(&prompts[1]).contains("first unique instruction"));
    assert!(decode(&prompts[0]).ends_with("<think>\n\n</think>\n\n"));
    assert_eq!(result.answers.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), ["z", "a"]);
}
#[tokio::test]
async fn later_invalid_prompt_prevents_all_gpu_work() {
    let (instance, backend) = fixture(None, Some(1500), true);
    let error = instance
        .decide(
            request(vec![question("first", "short"), question("second", &"long ".repeat(3000))]),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, decision::Error::ContextOverflow));
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn cancellation_finishes_active_question_without_starting_the_next() {
    let cancel = CancellationToken::new();
    let (instance, backend) = fixture(Some(cancel.clone()), None, true);
    assert!(matches!(
        instance.decide(request(vec![question("first", "short"), question("second", "next")]), cancel).await,
        Err(decision::Error::Cancelled)
    ));
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn default_unsupported_backend_returns_typed_failure_without_generation() {
    let (mut instance, backend) = fixture(None, None, true);
    instance.kind = ChatInstanceKind::Token(Arc::new(NoScoring(backend.clone())));
    assert!(matches!(
        instance.decide(request(vec![question("first", "short")]), CancellationToken::new()).await,
        Err(decision::Error::Scoring(TokenScoreError::Unsupported))
    ));
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn invalid_numeric_output_is_a_typed_failure() {
    for values in [vec![], vec![0.0, 0.0], vec![f64::NAN], vec![f64::INFINITY], vec![f64::NEG_INFINITY]] {
        let (instance, backend) = fixture(None, None, true);
        *backend.malformed.lock().unwrap() = Some(values);
        let result = instance.decide(request(vec![question("first", "short")]), CancellationToken::new()).await;
        assert!(matches!(result, Err(decision::Error::Scoring(TokenScoreError::InvalidModelOutput(_)))));
    }
}
struct NoScoring(Arc<Numeric>);
impl BackendInstance for NoScoring {
    type StreamConfig = ChatReplyConfig;
    type StreamInput = StreamInput;
    type StreamOutput = StreamOutput;
    type StreamMetrics = Option<TokenStreamMetrics>;
    fn state(&self) -> Pin<Box<dyn Future<Output = Result<Box<dyn State>, Error>> + Send + '_>> {
        panic!("unsupported decisions must not allocate generation state")
    }
    fn stream<'a>(
        &'a self,
        _: &'a StreamInput,
        _: &'a mut dyn State,
        _: ChatReplyConfig,
        _: CancellationToken,
    ) -> Pin<Box<dyn InstanceStream<Item = Result<StreamOutput, Error>, Metrics = Self::StreamMetrics> + Send + 'a>>
    {
        panic!("unsupported decisions must not generate")
    }
    fn peak_memory_usage(&self) -> Option<usize> {
        None
    }
}
impl Instance for NoScoring {
    fn with_config(
        &self,
        _: ChatConfig,
    ) -> Box<dyn Instance> {
        panic!("must not create a second instance")
    }
    fn tokenizer(&self) -> Arc<Tokenizer> {
        self.0.tokenizer()
    }
    fn max_context_length(&self) -> Option<usize> {
        self.0.max_context_length()
    }
    fn stop_token_ids(&self) -> Option<Box<[u64]>> {
        None
    }
    fn sampling_defaults(&self) -> SamplingParameters {
        panic!("unsupported decisions must not sample")
    }
}
struct MessageOnly;
impl BackendInstance for MessageOnly {
    type StreamConfig = ChatReplyConfig;
    type StreamInput = shoji::traits::backend::chat_message::StreamInput;
    type StreamOutput = shoji::traits::backend::chat_message::StreamOutput;
    type StreamMetrics = shoji::traits::backend::chat_message::StreamMetrics;
    fn state(&self) -> Pin<Box<dyn Future<Output = Result<Box<dyn State>, Error>> + Send + '_>> {
        panic!("unsupported decisions must not allocate generation state")
    }
    fn stream<'a>(
        &'a self,
        _: &'a Self::StreamInput,
        _: &'a mut dyn State,
        _: ChatReplyConfig,
        _: CancellationToken,
    ) -> Pin<Box<dyn InstanceStream<Item = Result<Self::StreamOutput, Error>, Metrics = Self::StreamMetrics> + Send + 'a>>
    {
        panic!("unsupported decisions must not generate")
    }
    fn peak_memory_usage(&self) -> Option<usize> {
        None
    }
}
#[tokio::test]
async fn message_only_backend_is_unsupported_without_encoding_or_generation() {
    let (mut instance, _) = fixture(None, None, true);
    instance.kind = ChatInstanceKind::Message(Arc::new(MessageOnly));
    instance.model.encoding = None;
    assert!(matches!(
        instance.decide(request(vec![question("first", "short")]), CancellationToken::new()).await,
        Err(decision::Error::Scoring(TokenScoreError::Unsupported))
    ));
}
