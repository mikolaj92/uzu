use super::*;
#[test]
fn parses_ordered_questions_and_options_without_null_loss() {
    let request=parse(r#"{"state":{"nested":[null,true,2]},"questions":{"z":{"type":"choice","instructions":null,"criteria":{"z":null,"a":{"text":"second"}}},"a":{"type":"score","criteria":[null,"high"]},"":{"type":"noul","instructions":""}}}"#,"model").unwrap();
    assert_eq!(request.questions.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(), ["z", "a", ""]);
    let Question::Choice {
        outcomes,
        ..
    } = &request.questions[0].question
    else {
        panic!()
    };
    assert_eq!(outcomes.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), ["z", "a"]);
    assert!(outcomes[0].description.is_none());
    let Question::Score {
        levels,
        ..
    } = &request.questions[1].question
    else {
        panic!()
    };
    assert!(levels[0].is_none());
}
#[test]
fn rejects_duplicate_keys_at_every_depth_and_strict_extensions() {
    for body in [
        r#"{"state":"a","state":"b","questions":{}}"#,
        r#"{"state":{"x":1,"x":2},"questions":{}}"#,
        r#"{"state":"a","questions":{"q":{"type":"choice","criteria":{"a":null,"a":null}}}}"#,
    ] {
        assert_eq!(parse(body, "model").unwrap_err().code, "duplicate_key");
    }
    for (extra, code) in [
        ("\"model\":\"other\"", "model_not_found"),
        ("\"unknown\":true", "unknown_field"),
        ("\"assets\":{\"x\":{}}", "unsupported_feature"),
        ("\"stream\":true", "unsupported_feature"),
    ] {
        let body = format!(r#"{{"state":"","questions":{{"q":{{"type":"noul","instructions":""}}}},{extra}}}"#);
        assert_eq!(parse(&body, "model").unwrap_err().code, code, "{body}");
    }
}
#[test]
fn validates_cardinality_content_and_noul_criteria() {
    for value in ["null", "true", "1"] {
        let body = format!(r#"{{"state":{value},"questions":{{"q":{{"type":"noul","instructions":"yes"}}}}}}"#);
        assert_eq!(parse(&body, "model").unwrap_err().code, "invalid_request");
    }
    let request = parse(
        r#"{"state":"","assets":{},"questions":{"q":{"type":"noul","criteria":{"true":{},"false":null}}}}"#,
        "model",
    )
    .unwrap();
    assert_eq!(request.questions.len(), 1);
    assert!(parse(r#"{"state":"","questions":{"q":{"type":"noul","instructions":null}}}"#, "model").is_err());
    assert!(parse(r#"{"state":"","questions":{"q":{"type":"choice","criteria":{"only":null}}}}"#, "model").is_ok());
    for question in [
        json!({"type":"choice","criteria":{}}),
        json!({"type":"score","criteria":[null]}),
        json!({"type":"score","criteria":[null,null,null,null,null,null,null,null,null,null,null]}),
    ] {
        assert!(parse(&json!({"state":"","questions":{"q":question}}).to_string(), "model").is_err());
    }
}
#[test]
fn response_keeps_named_results_numeric_noul_and_zero_generated_usage() {
    let body = response(
        "actual-model",
        decision::Response {
            answers: vec![
                ("z".into(), Answer::Binary(0.7)),
                (
                    "a".into(),
                    Answer::Score {
                        score: 0.5,
                        confidence: 0.0,
                        legend: vec![None, Some(Content::new(json!("high")).unwrap())],
                        probabilities: vec![0.5, 0.5],
                    },
                ),
            ],
            input_tokens: 123,
        },
    );
    assert!(body.find("\"z\"").unwrap() < body.find("\"a\"").unwrap());
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["model"], "actual-model");
    assert_eq!(value["answers"]["z"]["noul"], 0.7);
    assert_eq!(value["answers"]["a"]["legend"]["0"], Value::Null);
    assert_eq!(value["usage"], json!({"input_tokens":123,"output_tokens":0}));
}
#[test]
fn unsupported_and_deadline_errors_have_real_late_statuses() {
    assert_eq!(
        domain_error(decision::Error::Scoring(shoji::traits::backend::chat_token::TokenScoreError::Unsupported)).status,
        Status::NotImplemented
    );
    assert_eq!(domain_error(decision::Error::Cancelled).status, Status::GatewayTimeout);
    let error: SystemOneResponse = failure(Status::TooManyRequests, "model_busy", "busy").into();
    assert!(error.retry);
    assert_eq!(error.status, Status::TooManyRequests);
}
