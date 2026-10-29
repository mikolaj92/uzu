use super::*;
fn content(text: &str) -> Content {
    Content::new(json!(text)).unwrap()
}
fn scores(p: &[f64]) -> TokenScores {
    TokenScores {
        log_probabilities: p.iter().map(|p| p.ln() - 1000.0).collect(),
        label_mass: 0.0,
    }
}
#[test]
fn official_answers_preserve_expectation_confidence_and_first_tie() {
    let question = Question::Score {
        instructions: None,
        levels: vec![None, None, None],
    };
    let Answer::Score {
        score,
        confidence,
        ..
    } = aggregate(question, scores(&[0.1, 0.2, 0.7])).unwrap()
    else {
        panic!()
    };
    assert!((score - 1.6).abs() < 1e-12);
    assert!((confidence - 0.4).abs() < 1e-12);
    let choice = || Question::Choice {
        instructions: None,
        outcomes: vec![
            Outcome {
                name: "z".into(),
                description: None,
            },
            Outcome {
                name: "a".into(),
                description: None,
            },
        ],
    };
    let Answer::Choice {
        choice,
        confidence,
        ..
    } = aggregate(choice(), scores(&[0.5, 0.5])).unwrap()
    else {
        panic!()
    };
    assert_eq!(choice, "z");
    assert_eq!(confidence, 0.0);
    let Answer::Binary(value) = aggregate(
        Question::Binary {
            instructions: Some(content("test")),
            on_true: None,
            on_false: None,
        },
        scores(&[0.8, 0.2]),
    )
    .unwrap() else {
        panic!()
    };
    assert!((value - 0.8).abs() < 1e-12);
    let Answer::Choice {
        confidence,
        ..
    } = aggregate(
        Question::Choice {
            instructions: None,
            outcomes: vec![Outcome {
                name: "only".into(),
                description: None,
            }],
        },
        scores(&[1.0]),
    )
    .unwrap()
    else {
        panic!()
    };
    assert_eq!(confidence, 1.0);
}
#[test]
fn names_are_not_prompt_instructions_and_null_descriptions_remain_null() {
    let question = Question::Choice {
        instructions: Some(content("choose")),
        outcomes: vec![
            Outcome {
                name: "z".into(),
                description: None,
            },
            Outcome {
                name: "a".into(),
                description: Some(Content::new(json!({"nested":["data"]})).unwrap()),
            },
        ],
    };
    let (record, labels) = prompt_record(&content("state"), &question);
    let record: Value = serde_json::from_str(&record).unwrap();
    assert_eq!(labels, vec!["A", "B"]);
    assert_eq!(record["question"]["outcomes"][0]["description"], Value::Null);
    assert_eq!(record["question"]["outcomes"][1]["description"], json!({"nested":["data"]}));
    let request = Request {
        state: content("state"),
        questions: vec![NamedQuestion {
            name: "arbitrary".into(),
            question,
        }],
    };
    assert!(request.validate().is_ok());
    assert!(!record.to_string().contains("arbitrary"));
}
#[test]
fn domain_rejects_invalid_shapes_but_preserves_arbitrary_json_data() {
    for value in [Value::Null, json!(true), json!(2)] {
        assert!(Content::new(value).is_err());
    }
    assert!(Content::new(json!({"type":"audio","status":"transcribed"})).is_ok());
    let request = Request {
        state: content("state"),
        questions: vec![NamedQuestion {
            name: "".into(),
            question: Question::Binary {
                instructions: None,
                on_true: None,
                on_false: None,
            },
        }],
    };
    assert!(request.validate().is_err());
}
#[test]
fn contextual_proxies_require_exact_single_token_extensions() {
    use tokenizers::{Tokenizer, models::wordlevel::WordLevel, pre_tokenizers::whitespace::Whitespace};
    let vocab =
        [("[UNK]".to_string(), 0), ("prompt".into(), 1), ("A".into(), 2), ("B".into(), 3)].into_iter().collect();
    let mut tokenizer = Tokenizer::new(WordLevel::builder().vocab(vocab).unk_token("[UNK]".into()).build().unwrap());
    tokenizer.with_pre_tokenizer(Some(Whitespace));
    assert_eq!(contextual_labels(&tokenizer, "prompt ", &[1], &["A".into(), "B".into()]).unwrap(), [2, 3]);
    assert!(matches!(
        contextual_labels(&tokenizer, "prompt ", &[99], &["A".into()]),
        Err(Error::UnsupportedPromptCodec(_))
    ));
    assert!(matches!(
        contextual_labels(&tokenizer, "prompt ", &[1], &["A B".into()]),
        Err(Error::UnsupportedLabelTokenization)
    ));
    assert!(matches!(
        contextual_labels(&tokenizer, "prompt ", &[1], &["unknown".into(), "missing".into()]),
        Err(Error::UnsupportedLabelTokenization)
    ));
    assert!(matches!(
        contextual_labels(&tokenizer, "prompt", &[1], &["A".into()]),
        Err(Error::UnsupportedLabelTokenization)
    ));
}
