use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn reasoning(effort: ReasoningEffort) -> Reasoning {
    Reasoning {
        effort: Some(effort),
        summary: None,
        context: None,
    }
}

/// A body shaped like the one `chat_body_from_responses_request` produces, including the
/// projected effort and the default output cap.
fn built_body() -> Value {
    json!({
        "model": "qwen",
        "stream": true,
        "max_completion_tokens": 131_072,
        "stream_options": {"include_usage": true},
        "reasoning_effort": "high",
    })
}

#[test]
fn projects_reasoning_effort_onto_the_chat_provider_vocabulary() {
    let projected = |effort: ReasoningEffort| chat_wire_reasoning_effort(Some(&reasoning(effort)));
    // The three values every major chat provider documents, so no projection is
    // rejected outright.
    assert_eq!(projected(ReasoningEffort::Minimal).as_deref(), Some("low"));
    assert_eq!(projected(ReasoningEffort::Low).as_deref(), Some("low"));
    assert_eq!(projected(ReasoningEffort::Medium).as_deref(), Some("high"));
    assert_eq!(projected(ReasoningEffort::High).as_deref(), Some("high"));
    assert_eq!(projected(ReasoningEffort::XHigh).as_deref(), Some("high"));
    assert_eq!(projected(ReasoningEffort::Max).as_deref(), Some("max"));
    assert_eq!(projected(ReasoningEffort::Ultra).as_deref(), Some("max"));
    // There is no portable spelling of "stop thinking" or "keep thinking across turns",
    // and no guessing at a wire value this client does not know.
    assert_eq!(projected(ReasoningEffort::None), None);
    assert_eq!(projected(ReasoningEffort::Persistent), None);
    assert_eq!(
        projected(ReasoningEffort::Custom("disabled".to_string())),
        None,
        "the Responses-only spelling of persistent is not a chat effort"
    );
    assert_eq!(chat_wire_reasoning_effort(None), None);
    assert_eq!(
        chat_wire_reasoning_effort(Some(&Reasoning {
            effort: None,
            summary: None,
            context: None,
        })),
        None
    );
}

#[test]
fn merges_extra_body_fields() {
    let mut body = json!({"model": "qwen"});
    merge_extra_body(
        &mut body,
        &HashMap::from([
            ("enable_thinking".to_string(), json!(true)),
            ("thinking_budget".to_string(), json!(1024)),
        ]),
    )
    .expect("extra body should merge");

    assert_eq!(body["enable_thinking"], true);
    assert_eq!(body["thinking_budget"], 1024);
}

#[test]
fn merges_stream_options_without_disabling_usage() {
    let mut body = json!({
        "model": "qwen",
        "stream_options": {"include_usage": true}
    });
    merge_extra_body(
        &mut body,
        &HashMap::from([(
            "stream_options".to_string(),
            json!({
                "include_usage": false,
                "provider_option": true
            }),
        )]),
    )
    .expect("extra body should merge");

    assert_eq!(
        body["stream_options"],
        json!({
            "include_usage": true,
            "provider_option": true
        })
    );
}

#[test]
fn rejects_a_non_object_stream_options_override() {
    let mut body = json!({
        "model": "qwen",
        "stream_options": {"include_usage": true}
    });
    // `stream_options` is special-cased before the null removal below, so it can be
    // rejected but never silently dropped: usage accounting depends on it.
    let error = merge_extra_body(
        &mut body,
        &HashMap::from([("stream_options".to_string(), json!(null))]),
    )
    .expect_err("a non-object stream_options must be rejected");
    assert!(
        matches!(&error, ApiError::Stream(message) if message.contains("stream_options")),
        "unexpected error: {error}"
    );
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
}

#[test]
fn extra_body_can_override_max_completion_tokens() {
    let mut body = built_body();
    let extra = HashMap::from([("max_completion_tokens".to_string(), json!(16384))]);
    merge_extra_body(&mut body, &extra).expect("merge should succeed");
    assert_eq!(body["max_completion_tokens"], 16384);
}

#[test]
fn null_removes_a_field_the_client_sent_and_sends_any_other_null() {
    let mut body = built_body();
    merge_extra_body(
        &mut body,
        &HashMap::from([
            // A model that does not reason opts out of the projected effort.
            (REASONING_EFFORT_KEY.to_string(), json!(null)),
            // A key this client never sends is passed through as an explicit null,
            // because a provider may require the key to be present.
            ("logprobs".to_string(), json!(null)),
        ]),
    )
    .expect("merge should succeed");

    assert!(
        body.get(REASONING_EFFORT_KEY).is_none(),
        "a catalog must be able to opt out of a field this client projects: {body}"
    );
    assert_eq!(body["logprobs"], json!(null));
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
}

#[test]
fn a_thinking_budget_drops_the_projected_reasoning_effort() {
    // The provider-level shape documented in the README: Qwen3.8 rejects a request
    // carrying both fields, and the budget already expresses the strength.
    let mut body = built_body();
    finalize_chat_body(
        &mut body,
        &HashMap::from([
            ("enable_thinking".to_string(), json!(true)),
            (THINKING_BUDGET_KEY.to_string(), json!(1024)),
        ]),
        &HashMap::new(),
    )
    .expect("finalize should succeed");

    assert!(
        body.get(REASONING_EFFORT_KEY).is_none(),
        "the rejected combination must not reach the provider: {body}"
    );
    assert_eq!(body[THINKING_BUDGET_KEY], 1024);
    assert_eq!(body["enable_thinking"], true);
}

#[test]
fn a_declared_reasoning_effort_survives_a_thinking_budget() {
    // Both fields together are explicit configuration then, so the pairing is left
    // alone whichever layer declared the effort.
    for (provider_extra_body, model_extra_body) in [
        (
            HashMap::from([
                (THINKING_BUDGET_KEY.to_string(), json!(1024)),
                (REASONING_EFFORT_KEY.to_string(), json!("xhigh")),
            ]),
            HashMap::new(),
        ),
        (
            HashMap::from([(THINKING_BUDGET_KEY.to_string(), json!(1024))]),
            HashMap::from([(REASONING_EFFORT_KEY.to_string(), json!("xhigh"))]),
        ),
    ] {
        let mut body = built_body();
        finalize_chat_body(&mut body, &provider_extra_body, &model_extra_body)
            .expect("finalize should succeed");
        assert_eq!(body[REASONING_EFFORT_KEY], json!("xhigh"));
        assert_eq!(body[THINKING_BUDGET_KEY], 1024);
    }
}

#[test]
fn a_null_thinking_budget_keeps_the_projected_reasoning_effort() {
    // Removing a budget is not declaring one: no conflicting pair reaches the wire, so
    // the projected effort must survive.
    let mut body = built_body();
    body[THINKING_BUDGET_KEY] = json!(262_144);
    finalize_chat_body(
        &mut body,
        &HashMap::from([(THINKING_BUDGET_KEY.to_string(), json!(262_144))]),
        &HashMap::from([(THINKING_BUDGET_KEY.to_string(), json!(null))]),
    )
    .expect("finalize should succeed");

    assert!(
        body.get(THINKING_BUDGET_KEY).is_none(),
        "the model layer removes what the provider layer declared: {body}"
    );
    assert_eq!(body[REASONING_EFFORT_KEY], json!("high"));
}
