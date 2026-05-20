#![allow(clippy::unwrap_used)]

use std::collections::HashMap;

use anyhow::Result;
use codex_core::StartThreadOptions;
use codex_model_provider_info::WireApi;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::mount_chat_sse_once;
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

fn chat_sse(chunks: Vec<Value>) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn chat_text_response(response_id: &str, text: &str) -> String {
    chat_sse(vec![json!({
        "id": response_id,
        "choices": [{
            "delta": {"content": text},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "total_tokens": 2
        }
    })])
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

async fn submit_user_turn(test: &TestCodex, prompt: &str) -> Result<()> {
    test.submit_turn(prompt).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_wire_api_posts_to_chat_completions_and_merges_extra_body() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = mount_chat_sse_once(&server, chat_text_response("chatcmpl-1", "done")).await;
    let test = {
        let mut builder = test_codex().with_config({
            let base_url = format!("{}/v1", server.uri());
            move |config| {
                config.model_provider.base_url = Some(base_url);
                config.model_provider.wire_api = WireApi::Chat;
                config.model_provider.supports_websockets = false;
                config.model_provider.extra_body = Some(HashMap::from([(
                    "enable_thinking".to_string(),
                    json!(true),
                )]));
            }
        });
        builder.build(&server).await?
    };

    submit_user_turn(&test, "hello").await?;

    let request = mock.single_request();
    let body = request.body_json();
    assert_eq!(request.path(), "/v1/chat/completions");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(body["enable_thinking"], true);
    assert_eq!(body["messages"][0]["role"], "system");
    assert!(
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message == &json!({"role": "user", "content": "hello"}))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_wire_api_handles_reasoning_stream() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    mount_chat_sse_once(
        &server,
        chat_sse(vec![json!({
            "id": "chatcmpl-2",
            "choices": [{
                "delta": {"reasoning_content": "thinking", "content": "answer"},
                "finish_reason": "stop"
            }]
        })]),
    )
    .await;
    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Chat;
            config.model_provider.supports_websockets = false;
        }
    });
    let test = builder.build(&server).await?;

    submit_user_turn(&test, "think").await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_wire_api_tool_call_round_trip_sends_tool_message() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    mount_chat_sse_once(
        &server,
        chat_sse(vec![json!({
            "id": "chatcmpl-tool-1",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-1",
                        "function": {
                            "name": "shell_command",
                            "arguments": "{\"command\":\"echo chat-tool\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })]),
    )
    .await;
    let follow_up =
        mount_chat_sse_once(&server, chat_text_response("chatcmpl-tool-2", "done")).await;
    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Chat;
            config.model_provider.supports_websockets = false;
        }
    });
    let test = builder.build(&server).await?;

    submit_user_turn(&test, "use a tool").await?;

    let body = follow_up.single_request().body_json();
    assert!(body["messages"].as_array().unwrap().iter().any(|message| {
        message["role"] == "tool"
            && message["tool_call_id"] == "call-1"
            && message["content"]
                .as_str()
                .unwrap_or_default()
                .contains("chat-tool")
    }));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_wire_api_namespaced_dynamic_tool_round_trip() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let first_request = mount_chat_sse_once(
        &server,
        chat_sse(vec![json!({
            "id": "chatcmpl-dynamic-1",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-dynamic",
                        "function": {
                            "name": "codex_app_lookup_order",
                            "arguments": "{\"id\":\"ord_123\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })]),
    )
    .await;
    let follow_up =
        mount_chat_sse_once(&server, chat_text_response("chatcmpl-dynamic-2", "done")).await;
    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Chat;
            config.model_provider.supports_websockets = false;
        }
    });
    let base_test = builder.build(&server).await?;
    let new_thread = base_test
        .thread_manager
        .start_thread(StartThreadOptions {
            config: base_test.config.clone(),
            dynamic_tools: vec![DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
                name: "codex_app".to_string(),
                description: "Codex app tools.".to_string(),
                tools: vec![DynamicToolNamespaceTool::Function(
                    DynamicToolFunctionSpec {
                        name: "lookup_order".to_string(),
                        description: "Lookup an order.".to_string(),
                        input_schema: json!({
                            "type": "object",
                            "properties": {"id": {"type": "string"}},
                            "required": ["id"],
                            "additionalProperties": false
                        }),
                        defer_loading: false,
                    },
                )],
            })],
            ..StartThreadOptions::new(base_test.config.clone())
        })
        .await?;
    let mut test = base_test;
    test.codex = new_thread.thread;
    test.session_configured = new_thread.session_configured;

    submit_user_turn_without_waiting(&test, "lookup order").await?;

    let EventMsg::DynamicToolCallRequest(request) = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::DynamicToolCallRequest(_))
    })
    .await
    else {
        unreachable!("event guard guarantees DynamicToolCallRequest");
    };
    assert_eq!(request.call_id, "call-dynamic");
    assert_eq!(request.namespace.as_deref(), Some("codex_app"));
    assert_eq!(request.tool, "lookup_order");
    assert_eq!(request.arguments, json!({"id": "ord_123"}));

    test.codex
        .submit(Op::DynamicToolResponse {
            id: request.call_id,
            response: DynamicToolResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputText {
                    text: "order found".to_string(),
                }],
                success: true,
            },
        })
        .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let first_body = first_request.single_request().body_json();
    assert!(first_body["tools"].as_array().unwrap().iter().any(|tool| {
        tool["type"] == "function" && tool["function"]["name"] == "codex_app_lookup_order"
    }));

    let follow_up_body = follow_up.single_request().body_json();
    assert!(
        follow_up_body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "assistant"
                    && message["tool_calls"]
                        == json!([{
                            "id": "call-dynamic",
                            "type": "function",
                            "function": {
                                "name": "codex_app_lookup_order",
                                "arguments": "{\"id\":\"ord_123\"}"
                            }
                        }])
            })
    );
    assert!(
        follow_up_body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "tool"
                    && message["tool_call_id"] == "call-dynamic"
                    && message["content"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("order found")
            })
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_wire_api_parallel_tool_calls_round_trip_in_one_assistant_message() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    mount_chat_sse_once(
        &server,
        chat_sse(vec![json!({
            "id": "chatcmpl-tool-1",
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call-alpha",
                            "function": {
                                "name": "shell_command",
                                "arguments": "{\"command\":\"echo alpha\"}"
                            }
                        },
                        {
                            "index": 1,
                            "id": "call-beta",
                            "function": {
                                "name": "shell_command",
                                "arguments": "{\"command\":\"echo beta\"}"
                            }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        })]),
    )
    .await;
    let follow_up =
        mount_chat_sse_once(&server, chat_text_response("chatcmpl-tool-2", "done")).await;
    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Chat;
            config.model_provider.supports_websockets = false;
        }
    });
    let test = builder.build(&server).await?;

    submit_user_turn(&test, "use two tools").await?;

    let body = follow_up.single_request().body_json();
    let messages = body["messages"].as_array().unwrap();
    let assistant_tool_messages = messages
        .iter()
        .filter(|message| message["role"] == "assistant" && message.get("tool_calls").is_some())
        .collect::<Vec<_>>();
    assert_eq!(assistant_tool_messages.len(), 1);
    assert_eq!(
        assistant_tool_messages[0]["tool_calls"],
        json!([
            {
                "id": "call-alpha",
                "type": "function",
                "function": {
                    "name": "shell_command",
                    "arguments": "{\"command\":\"echo alpha\"}"
                }
            },
            {
                "id": "call-beta",
                "type": "function",
                "function": {
                    "name": "shell_command",
                    "arguments": "{\"command\":\"echo beta\"}"
                }
            }
        ])
    );
    assert!(messages.iter().any(|message| {
        message["role"] == "tool"
            && message["tool_call_id"] == "call-alpha"
            && message["content"]
                .as_str()
                .unwrap_or_default()
                .contains("alpha")
    }));
    assert!(messages.iter().any(|message| {
        message["role"] == "tool"
            && message["tool_call_id"] == "call-beta"
            && message["content"]
                .as_str()
                .unwrap_or_default()
                .contains("beta")
    }));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_wire_api_emits_token_usage_event() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    mount_chat_sse_once(
        &server,
        chat_sse(vec![
            json!({
                "id": "chatcmpl-usage-1",
                "choices": [{
                    "delta": {"content": "usage"},
                    "finish_reason": "stop"
                }]
            }),
            json!({
                "id": "chatcmpl-usage-1",
                "choices": [],
                "usage": {
                    "prompt_tokens": 11,
                    "completion_tokens": 7,
                    "total_tokens": 18,
                    "prompt_tokens_details": {"cached_tokens": 4},
                    "completion_tokens_details": {"reasoning_tokens": 3}
                }
            }),
        ]),
    )
    .await;
    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Chat;
            config.model_provider.supports_websockets = false;
            config.model_auto_compact_token_limit = Some(200);
        }
    });
    let test = builder.build(&server).await?;

    submit_user_turn_without_waiting(&test, "usage").await?;

    let token_count_event = wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::TokenCount(token_count)
                if token_count
                    .info
                    .as_ref()
                    .is_some_and(|info| {
                        info.last_token_usage.input_tokens == 11
                            && info.last_token_usage.cached_input_tokens == 4
                            && info.last_token_usage.output_tokens == 7
                            && info.last_token_usage.reasoning_output_tokens == 3
                            && info.last_token_usage.total_tokens == 18
                    })
        )
    })
    .await;
    assert!(matches!(token_count_event, EventMsg::TokenCount(_)));
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_wire_api_token_usage_drives_auto_compaction() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    mount_chat_sse_once(
        &server,
        chat_sse(vec![json!({
            "id": "chatcmpl-usage-1",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-compact",
                        "function": {
                            "name": "shell_command",
                            "arguments": "{\"command\":\"echo compact-trigger\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 50000000,
                "completion_tokens": 50000000,
                "total_tokens": 100000000,
                "prompt_tokens_details": {"cached_tokens": 1234},
                "completion_tokens_details": {"reasoning_tokens": 5678}
            }
        })]),
    )
    .await;
    let compact = mount_chat_sse_once(
        &server,
        chat_text_response("chatcmpl-compact", "CHAT_COMPACTED_SUMMARY"),
    )
    .await;
    let follow_up = mount_chat_sse_once(
        &server,
        chat_text_response("chatcmpl-usage-2", "after compact"),
    )
    .await;
    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Chat;
            config.model_provider.supports_websockets = false;
            config.model_auto_compact_token_limit = Some(200);
        }
    });
    let test = builder.build(&server).await?;

    submit_user_turn_without_waiting(&test, "trigger compact").await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ContextCompacted(_))
    })
    .await;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    assert_eq!(compact.requests().len(), 1);
    let follow_up_body = follow_up.single_request().body_json().to_string();
    assert!(
        follow_up_body.contains("CHAT_COMPACTED_SUMMARY"),
        "expected follow-up chat request to include compacted summary, got {follow_up_body}"
    );

    Ok(())
}
