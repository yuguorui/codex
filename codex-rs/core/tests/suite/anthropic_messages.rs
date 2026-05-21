#![allow(clippy::unwrap_used)]

use anyhow::Result;
use codex_model_provider_info::WireApi;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::mount_anthropic_sse_once;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;

fn anthropic_sse(events: Vec<Value>) -> String {
    let mut body = String::new();
    for event in events {
        body.push_str(&format!("data: {event}\n\n"));
    }
    body
}

fn anthropic_text_response(response_id: &str, text: &str) -> String {
    anthropic_sse(vec![
        json!({
            "type": "message_start",
            "message": {
                "id": response_id,
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }
        }),
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        }),
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": text}
        }),
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 2}
        }),
        json!({"type": "message_stop"}),
    ])
}

async fn submit_user_turn(test: &TestCodex, prompt: &str) -> Result<()> {
    test.submit_turn(prompt).await?;
    Ok(())
}

async fn submit_user_turn_without_waiting(test: &TestCodex, prompt: &str) -> Result<()> {
    let session_model = test.session_configured.model.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.cwd.path());

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(test.config.cwd.clone())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: session_model,
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_wire_api_posts_to_messages_and_merges_extra_body() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = mount_anthropic_sse_once(&server, anthropic_text_response("msg-1", "done")).await;
    let test = {
        let mut builder = test_codex().with_config({
            let base_url = format!("{}/v1", server.uri());
            move |config| {
                config.model_provider.base_url = Some(base_url);
                config.model_provider.wire_api = WireApi::Anthropic;
                config.model_provider.supports_websockets = false;
                config.model_provider.extra_body = Some(HashMap::from([(
                    "metadata".to_string(),
                    json!({"user_id": "u"}),
                )]));
            }
        });
        builder.build(&server).await?
    };

    submit_user_turn(&test, "hello").await?;

    let request = mock.single_request();
    let body = request.body_json();
    assert_eq!(request.path(), "/v1/messages");
    assert_eq!(
        request.header("anthropic-version").as_deref(),
        Some("2023-06-01")
    );
    assert_eq!(body["stream"], true);
    assert_eq!(body["model"], test.session_configured.model);
    assert!(
        body["system"]
            .as_str()
            .is_some_and(|system| system.contains("system"))
    );
    assert_eq!(body["metadata"], json!({"user_id": "u"}));
    assert!(
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message
                == &json!({
                    "role": "user",
                    "content": [{"type": "text", "text": "hello"}]
                }))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_wire_api_tool_call_round_trip_sends_tool_result() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let first = mount_anthropic_sse_once(
        &server,
        anthropic_sse(vec![
            json!({
                "type": "message_start",
                "message": {"id": "msg-tool", "usage": {"input_tokens": 1, "output_tokens": 1}}
            }),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": "call-1",
                    "name": "shell_command",
                    "input": {}
                }
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{\"command\":\"echo anthropic-tool\"}"}
            }),
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use"},
                "usage": {"output_tokens": 5}
            }),
            json!({"type": "message_stop"}),
        ]),
    )
    .await;
    let second =
        mount_anthropic_sse_once(&server, anthropic_text_response("msg-final", "done")).await;

    let test = {
        let mut builder = test_codex().with_config({
            let base_url = format!("{}/v1", server.uri());
            move |config| {
                config.model_provider.base_url = Some(base_url);
                config.model_provider.wire_api = WireApi::Anthropic;
                config.model_provider.supports_websockets = false;
            }
        });
        builder.build(&server).await?
    };

    submit_user_turn_without_waiting(&test, "run tool").await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let first_request = first.single_request();
    assert_eq!(first_request.path(), "/v1/messages");

    let second_body = second.single_request().body_json();
    assert!(
        second_body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| { message["content"].as_array().into_iter().flatten() })
            .any(|content| {
                content["type"] == "tool_result"
                    && content["tool_use_id"] == "call-1"
                    && content["content"]
                        .as_str()
                        .is_some_and(|text| text.contains("anthropic-tool"))
            }),
        "missing Anthropic tool_result in follow-up body: {second_body:#}"
    );
    Ok(())
}
