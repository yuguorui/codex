use super::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::AbsolutePathBufGuard;
use pretty_assertions::assert_eq;
use std::num::NonZeroU64;
use tempfile::tempdir;

#[test]
fn test_deserialize_ollama_model_provider_toml() {
    let azure_provider_toml = r#"
name = "Ollama"
base_url = "http://localhost:11434/v1"
        "#;
    let expected_provider = ModelProviderInfo {
        name: "Ollama".into(),
        base_url: Some("http://localhost:11434/v1".into()),
        env_key: None,
        env_key_auth: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        aws: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        extra_headers: None,
        env_http_headers: None,
        env_extra_headers: None,
        extra_body: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        supports_standalone_web_search: false,
    };

    let provider: ModelProviderInfo = toml::from_str(azure_provider_toml).unwrap();
    assert_eq!(expected_provider, provider);
}

#[test]
fn test_deserialize_azure_model_provider_toml() {
    let azure_provider_toml = r#"
name = "Azure"
base_url = "https://xxxxx.openai.azure.com/openai"
env_key = "AZURE_OPENAI_API_KEY"
query_params = { api-version = "2025-04-01-preview" }
        "#;
    let expected_provider = ModelProviderInfo {
        name: "Azure".into(),
        base_url: Some("https://xxxxx.openai.azure.com/openai".into()),
        env_key: Some("AZURE_OPENAI_API_KEY".into()),
        env_key_auth: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        aws: None,
        wire_api: WireApi::Responses,
        query_params: Some(maplit::hashmap! {
            "api-version".to_string() => "2025-04-01-preview".to_string(),
        }),
        http_headers: None,
        extra_headers: None,
        env_http_headers: None,
        env_extra_headers: None,
        extra_body: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        supports_standalone_web_search: false,
    };

    let provider: ModelProviderInfo = toml::from_str(azure_provider_toml).unwrap();
    assert_eq!(expected_provider, provider);
}

#[test]
fn test_deserialize_example_model_provider_toml() {
    let azure_provider_toml = r#"
name = "Example"
base_url = "https://example.com"
env_key = "API_KEY"
http_headers = { "X-Example-Header" = "example-value" }
env_http_headers = { "X-Example-Env-Header" = "EXAMPLE_ENV_VAR" }
supports_standalone_web_search = true
        "#;
    let expected_provider = ModelProviderInfo {
        name: "Example".into(),
        base_url: Some("https://example.com".into()),
        env_key: Some("API_KEY".into()),
        env_key_auth: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        aws: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: Some(maplit::hashmap! {
            "X-Example-Header".to_string() => "example-value".to_string(),
        }),
        extra_headers: None,
        env_http_headers: Some(maplit::hashmap! {
            "X-Example-Env-Header".to_string() => "EXAMPLE_ENV_VAR".to_string(),
        }),
        env_extra_headers: None,
        extra_body: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        supports_standalone_web_search: true,
    };

    let provider: ModelProviderInfo = toml::from_str(azure_provider_toml).unwrap();
    assert_eq!(expected_provider, provider);
}

#[test]
fn test_deserialize_chat_wire_api() {
    let provider_toml = r#"
name = "OpenAI using Chat Completions"
base_url = "https://api.openai.com/v1"
env_key = "OPENAI_API_KEY"
wire_api = "chat"
        "#;

    let provider: ModelProviderInfo = toml::from_str(provider_toml).unwrap();
    assert_eq!(provider.wire_api, WireApi::Chat);
}

#[test]
fn test_deserialize_anthropic_wire_api() {
    let provider_toml = r#"
name = "Anthropic"
base_url = "https://api.anthropic.com/v1"
env_key = "ANTHROPIC_API_KEY"
wire_api = "anthropic"
requires_openai_auth = false
        "#;

    let provider: ModelProviderInfo = toml::from_str(provider_toml).unwrap();
    assert_eq!(provider.wire_api, WireApi::Anthropic);
}

#[test]
fn test_deserialize_env_key_auth() {
    let provider_toml = r#"
name = "Anthropic Compatible"
base_url = "https://example.com/anthropic/v1"
env_key = "ANTHROPIC_COMPAT_API_KEY"
env_key_auth = "bearer"
wire_api = "anthropic"
requires_openai_auth = false
        "#;

    let provider: ModelProviderInfo = toml::from_str(provider_toml).unwrap();
    assert_eq!(provider.env_key_auth, Some(EnvKeyAuthScheme::Bearer));
}

#[test]
fn test_anthropic_thinking_override_via_extra_body() {
    // Default: no thinking override, provider uses adaptive (the new default).
    let provider_toml = r#"
name = "Anthropic"
base_url = "https://api.anthropic.com/v1"
env_key = "ANTHROPIC_API_KEY"
wire_api = "anthropic"
requires_openai_auth = false
        "#;
    let provider: ModelProviderInfo = toml::from_str(provider_toml).unwrap();
    assert_eq!(provider.wire_api, WireApi::Anthropic);
    // Without extra_body, the request defaults to { type: "adaptive" }.
    assert!(provider.extra_body.is_none());

    // Override to enabled with a custom token budget via extra_body.
    let provider_toml = r#"
name = "Anthropic Override"
base_url = "https://api.anthropic.com/v1"
env_key = "ANTHROPIC_API_KEY"
wire_api = "anthropic"
requires_openai_auth = false
extra_body = { thinking = { type = "enabled", budget_tokens = 16000 } }
        "#;
    let provider: ModelProviderInfo = toml::from_str(provider_toml).unwrap();
    assert_eq!(
        provider.extra_body,
        Some(maplit::hashmap! {
            "thinking".to_string() => serde_json::json!({
                "type": "enabled",
                "budget_tokens": 16000,
            }),
        })
    );

    // Override to force adaptive even when reasoning effort would normally send "enabled".
    let provider_toml = r#"
name = "Anthropic Force Adaptive"
base_url = "https://api.anthropic.com/v1"
env_key = "ANTHROPIC_API_KEY"
wire_api = "anthropic"
requires_openai_auth = false
extra_body = { thinking = { type = "adaptive" } }
        "#;
    let provider: ModelProviderInfo = toml::from_str(provider_toml).unwrap();
    assert_eq!(
        provider.extra_body,
        Some(maplit::hashmap! {
            "thinking".to_string() => serde_json::json!({
                "type": "adaptive",
            }),
        })
    );
}

#[test]
fn test_deserialize_extra_headers_and_extra_body() {
    let provider_toml = r#"
name = "OpenAI Compatible"
base_url = "https://example.com/compatible-mode/v1"
wire_api = "chat"
requires_openai_auth = false
extra_headers = { "X-Provider-DataInspection" = '{"input":"cip","output":"cip"}' }
env_extra_headers = { "X-Custom-Token" = "CUSTOM_TOKEN_ENV" }
extra_body = { enable_thinking = true, thinking_budget = 1024 }
        "#;

    let provider: ModelProviderInfo = toml::from_str(provider_toml).unwrap();

    assert_eq!(
        provider.extra_headers,
        Some(maplit::hashmap! {
            "X-Provider-DataInspection".to_string() =>
                r#"{"input":"cip","output":"cip"}"#.to_string(),
        })
    );
    assert_eq!(
        provider.env_extra_headers,
        Some(maplit::hashmap! {
            "X-Custom-Token".to_string() => "CUSTOM_TOKEN_ENV".to_string(),
        })
    );
    assert_eq!(
        provider.extra_body,
        Some(maplit::hashmap! {
            "enable_thinking".to_string() => serde_json::json!(true),
            "thinking_budget".to_string() => serde_json::json!(1024),
        })
    );
}

#[test]
fn test_extra_headers_are_injected_into_api_provider() {
    unsafe {
        std::env::set_var("CODEX_TEST_CUSTOM_TOKEN_ENV", "secret-token");
    }
    let provider = ModelProviderInfo {
        extra_headers: Some(maplit::hashmap! {
            "X-Static".to_string() => "static-value".to_string(),
        }),
        env_extra_headers: Some(maplit::hashmap! {
            "X-Env".to_string() => "CODEX_TEST_CUSTOM_TOKEN_ENV".to_string(),
        }),
        ..ModelProviderInfo::default()
    };

    let api_provider = provider
        .to_api_provider(/*auth_mode*/ None)
        .expect("provider should build");

    assert_eq!(
        api_provider
            .headers
            .get("X-Static")
            .and_then(|value| value.to_str().ok()),
        Some("static-value")
    );
    assert_eq!(
        api_provider
            .headers
            .get("X-Env")
            .and_then(|value| value.to_str().ok()),
        Some("secret-token")
    );
    unsafe {
        std::env::remove_var("CODEX_TEST_CUSTOM_TOKEN_ENV");
    }
}

#[test]
fn test_deserialize_websocket_connect_timeout() {
    let provider_toml = r#"
name = "OpenAI"
base_url = "https://api.openai.com/v1"
websocket_connect_timeout_ms = 15000
supports_websockets = true
        "#;

    let provider: ModelProviderInfo = toml::from_str(provider_toml).unwrap();
    assert_eq!(provider.websocket_connect_timeout_ms, Some(15_000));
}

#[test]
fn test_supports_remote_compaction_for_openai() {
    let provider = ModelProviderInfo::create_openai_provider(/*base_url*/ None);

    assert!(provider.supports_remote_compaction());
}

#[test]
fn test_personal_access_token_uses_chatgpt_codex_base_url() {
    let api_provider = ModelProviderInfo::create_openai_provider(/*base_url*/ None)
        .to_api_provider(Some(AuthMode::PersonalAccessToken))
        .expect("OpenAI provider should build API provider");

    assert_eq!(api_provider.base_url, CHATGPT_CODEX_BASE_URL);
}

#[test]
fn test_header_auth_uses_chatgpt_codex_base_url() {
    let api_provider = ModelProviderInfo::create_openai_provider(/*base_url*/ None)
        .to_api_provider(Some(AuthMode::Headers))
        .expect("OpenAI provider should build API provider");

    assert_eq!(api_provider.base_url, CHATGPT_CODEX_BASE_URL);
}

#[test]
fn test_supports_remote_compaction_for_azure_name() {
    let provider = ModelProviderInfo {
        name: "Azure".into(),
        base_url: Some("https://example.com/openai".into()),
        env_key: Some("AZURE_OPENAI_API_KEY".into()),
        env_key_auth: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        aws: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        extra_headers: None,
        env_http_headers: None,
        env_extra_headers: None,
        extra_body: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        supports_standalone_web_search: false,
    };

    assert!(provider.supports_remote_compaction());
}

#[test]
fn test_chat_provider_does_not_support_responses_remote_compaction() {
    let provider = ModelProviderInfo {
        wire_api: WireApi::Chat,
        ..ModelProviderInfo::create_openai_provider(/*base_url*/ None)
    };

    assert!(!provider.supports_remote_compaction());
}

#[test]
fn test_supports_remote_compaction_for_non_openai_non_azure_provider() {
    let provider = ModelProviderInfo {
        name: "Example".into(),
        base_url: Some("https://example.com/v1".into()),
        env_key: Some("API_KEY".into()),
        env_key_auth: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        aws: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        extra_headers: None,
        env_http_headers: None,
        env_extra_headers: None,
        extra_body: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        supports_standalone_web_search: false,
    };

    assert!(!provider.supports_remote_compaction());
}

#[test]
fn test_uses_openai_actor_authorization() {
    let mut provider = ModelProviderInfo {
        http_headers: Some(maplit::hashmap! {
            "X-OpenAI-Actor-Authorization".to_string() => "actor-token".to_string(),
        }),
        ..ModelProviderInfo::default()
    };
    assert!(provider.uses_openai_actor_authorization());

    provider.http_headers = None;
    assert!(!provider.uses_openai_actor_authorization());

    provider.http_headers = Some(maplit::hashmap! {
        OPENAI_ACTOR_AUTHORIZATION_HEADER.to_string() => "  ".to_string(),
    });
    assert!(!provider.uses_openai_actor_authorization());

    provider.http_headers = Some(maplit::hashmap! {
        OPENAI_ACTOR_AUTHORIZATION_HEADER.to_string() => "actor-token".to_string(),
    });
    provider.requires_openai_auth = true;
    assert!(!provider.uses_openai_actor_authorization());
}

#[test]
fn test_deserialize_provider_auth_config_defaults() {
    let base_dir = tempdir().unwrap();
    let provider_toml = r#"
name = "Corp"

[auth]
command = "./scripts/print-token"
args = ["--format=text"]
        "#;

    let provider: ModelProviderInfo = {
        let _guard = AbsolutePathBufGuard::new(base_dir.path());
        toml::from_str(provider_toml).unwrap()
    };

    assert_eq!(
        provider.auth,
        Some(ModelProviderAuthInfo {
            command: "./scripts/print-token".to_string(),
            args: vec!["--format=text".to_string()],
            timeout_ms: NonZeroU64::new(5_000).unwrap(),
            refresh_interval_ms: 300_000,
            cwd: AbsolutePathBuf::resolve_path_against_base(".", base_dir.path()),
        })
    );
}

#[test]
fn test_deserialize_provider_aws_config() {
    let provider_toml = r#"
name = "Amazon Bedrock"
base_url = "https://bedrock.example.com/v1"

[aws]
profile = "codex-bedrock"
region = "us-west-2"
        "#;

    let provider: ModelProviderInfo = toml::from_str(provider_toml).unwrap();

    assert_eq!(
        provider.aws,
        Some(ModelProviderAwsAuthInfo {
            profile: Some("codex-bedrock".to_string()),
            region: Some("us-west-2".to_string()),
        })
    );
}

#[test]
fn test_create_amazon_bedrock_provider() {
    assert_eq!(
        ModelProviderInfo::create_amazon_bedrock_provider(/*aws*/ None),
        ModelProviderInfo {
            name: "Amazon Bedrock".to_string(),
            base_url: None,
            env_key: None,
            env_key_auth: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            auth: None,
            aws: Some(ModelProviderAwsAuthInfo {
                profile: None,
                region: None,
            }),
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: Some(maplit::hashmap! {
                AMAZON_BEDROCK_MANTLE_CLIENT_AGENT_HEADER.to_string() =>
                    AMAZON_BEDROCK_MANTLE_CLIENT_AGENT_VALUE.to_string(),
            }),
            extra_headers: None,
            env_http_headers: None,
            env_extra_headers: None,
            extra_body: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            websocket_connect_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
            supports_standalone_web_search: false,
        }
    );
}

fn provider_auth_for_test() -> ModelProviderAuthInfo {
    ModelProviderAuthInfo {
        command: "token-fetcher".to_string(),
        args: vec!["fetch".to_string()],
        timeout_ms: NonZeroU64::new(5_000).expect("timeout should be non-zero"),
        refresh_interval_ms: 300_000,
        cwd: std::env::current_dir()
            .expect("current directory should be available")
            .try_into()
            .expect("current directory should be absolute"),
    }
}

#[test]
fn test_amazon_bedrock_provider_adds_mantle_client_agent_header() {
    let api_provider = ModelProviderInfo::create_amazon_bedrock_provider(/*aws*/ None)
        .to_api_provider(/*auth_mode*/ None)
        .expect("Amazon Bedrock provider should build API provider");

    assert_eq!(
        api_provider
            .headers
            .get(AMAZON_BEDROCK_MANTLE_CLIENT_AGENT_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(AMAZON_BEDROCK_MANTLE_CLIENT_AGENT_VALUE)
    );
}

#[test]
fn test_built_in_model_providers_include_amazon_bedrock() {
    let providers = built_in_model_providers(/*openai_base_url*/ None);

    assert_eq!(
        providers
            .get(AMAZON_BEDROCK_PROVIDER_ID)
            .map(ModelProviderInfo::is_amazon_bedrock),
        Some(true)
    );
}

#[test]
fn test_merge_configured_model_providers_adds_custom_provider() {
    let custom_provider = ModelProviderInfo {
        name: "Custom".to_string(),
        base_url: Some("https://example.com/v1".to_string()),
        ..ModelProviderInfo::default()
    };
    let configured_model_providers =
        std::collections::HashMap::from([("custom".to_string(), custom_provider.clone())]);

    let mut expected = built_in_model_providers(/*openai_base_url*/ None);
    expected.insert("custom".to_string(), custom_provider);

    assert_eq!(
        merge_configured_model_providers(
            built_in_model_providers(/*openai_base_url*/ None),
            configured_model_providers,
        ),
        Ok(expected)
    );
}

#[test]
fn test_merge_configured_model_providers_applies_amazon_bedrock_profile_override() {
    let configured_model_providers = std::collections::HashMap::from([(
        AMAZON_BEDROCK_PROVIDER_ID.to_string(),
        ModelProviderInfo {
            aws: Some(ModelProviderAwsAuthInfo {
                profile: Some("codex-bedrock".to_string()),
                region: Some("us-west-2".to_string()),
            }),
            ..ModelProviderInfo::default()
        },
    )]);

    let mut expected = built_in_model_providers(/*openai_base_url*/ None);
    expected
        .get_mut(AMAZON_BEDROCK_PROVIDER_ID)
        .expect("Amazon Bedrock provider should be built in")
        .aws = Some(ModelProviderAwsAuthInfo {
        profile: Some("codex-bedrock".to_string()),
        region: Some("us-west-2".to_string()),
    });

    assert_eq!(
        merge_configured_model_providers(
            built_in_model_providers(/*openai_base_url*/ None),
            configured_model_providers,
        ),
        Ok(expected)
    );
}

#[test]
fn test_merge_configured_model_providers_applies_amazon_bedrock_transport_overrides() {
    let auth = provider_auth_for_test();
    let configured_model_providers = std::collections::HashMap::from([(
        AMAZON_BEDROCK_PROVIDER_ID.to_string(),
        ModelProviderInfo {
            base_url: Some("https://proxy.example.com/v1".to_string()),
            auth: Some(auth.clone()),
            aws: Some(ModelProviderAwsAuthInfo {
                profile: Some("codex-bedrock".to_string()),
                region: Some("us-west-2".to_string()),
            }),
            http_headers: Some(maplit::hashmap! {
                "x-example-header".to_string() => "value".to_string(),
            }),
            ..ModelProviderInfo::default()
        },
    )]);

    let mut expected = built_in_model_providers(/*openai_base_url*/ None);
    let expected_provider = expected
        .get_mut(AMAZON_BEDROCK_PROVIDER_ID)
        .expect("Amazon Bedrock provider should be built in");
    expected_provider.base_url = Some("https://proxy.example.com/v1".to_string());
    expected_provider.auth = Some(auth);
    expected_provider.aws = Some(ModelProviderAwsAuthInfo {
        profile: Some("codex-bedrock".to_string()),
        region: Some("us-west-2".to_string()),
    });
    expected_provider
        .http_headers
        .get_or_insert_default()
        .insert("x-example-header".to_string(), "value".to_string());

    assert_eq!(
        merge_configured_model_providers(
            built_in_model_providers(/*openai_base_url*/ None),
            configured_model_providers,
        ),
        Ok(expected)
    );
}

#[test]
fn test_merge_configured_model_providers_rejects_amazon_bedrock_non_default_fields() {
    let configured_model_providers = std::collections::HashMap::from([(
        AMAZON_BEDROCK_PROVIDER_ID.to_string(),
        ModelProviderInfo {
            name: "Custom Bedrock".to_string(),
            aws: Some(ModelProviderAwsAuthInfo {
                profile: Some("codex-bedrock".to_string()),
                region: None,
            }),
            ..ModelProviderInfo::default()
        },
    )]);

    assert_eq!(
        merge_configured_model_providers(
            built_in_model_providers(/*openai_base_url*/ None),
            configured_model_providers,
        ),
        Err(
            "model_providers.amazon-bedrock only supports changing `base_url`, `auth`, `http_headers`, `aws.profile`, and `aws.region`; other non-default provider fields are not supported"
                .to_string()
        )
    );
}

#[test]
fn test_merge_configured_model_providers_allows_amazon_bedrock_default_fields() {
    let configured_model_providers = std::collections::HashMap::from([(
        AMAZON_BEDROCK_PROVIDER_ID.to_string(),
        ModelProviderInfo {
            aws: Some(ModelProviderAwsAuthInfo {
                profile: None,
                region: None,
            }),
            wire_api: WireApi::Responses,
            ..ModelProviderInfo::default()
        },
    )]);

    assert_eq!(
        merge_configured_model_providers(
            built_in_model_providers(/*openai_base_url*/ None),
            configured_model_providers,
        ),
        Ok(built_in_model_providers(/*openai_base_url*/ None))
    );
}

#[test]
fn test_validate_provider_aws_rejects_conflicting_auth() {
    let provider = ModelProviderInfo {
        aws: Some(ModelProviderAwsAuthInfo {
            profile: None,
            region: None,
        }),
        env_key: Some("AWS_BEARER_TOKEN_BEDROCK".to_string()),
        supports_websockets: false,
        ..ModelProviderInfo::create_openai_provider(/*base_url*/ None)
    };

    assert_eq!(
        provider.validate(),
        Err("provider aws cannot be combined with env_key, requires_openai_auth".to_string())
    );
}

#[test]
fn test_validate_provider_aws_rejects_websockets() {
    let provider = ModelProviderInfo {
        aws: Some(ModelProviderAwsAuthInfo {
            profile: None,
            region: None,
        }),
        requires_openai_auth: false,
        supports_websockets: true,
        ..ModelProviderInfo::create_openai_provider(/*base_url*/ None)
    };

    assert_eq!(
        provider.validate(),
        Err("provider aws cannot be combined with supports_websockets".to_string())
    );
}

#[test]
fn test_deserialize_provider_auth_config_allows_zero_refresh_interval() {
    let base_dir = tempdir().unwrap();
    let provider_toml = r#"
name = "Corp"

[auth]
command = "./scripts/print-token"
refresh_interval_ms = 0
        "#;

    let provider: ModelProviderInfo = {
        let _guard = AbsolutePathBufGuard::new(base_dir.path());
        toml::from_str(provider_toml).unwrap()
    };

    let auth = provider.auth.expect("auth config should deserialize");
    assert_eq!(auth.refresh_interval_ms, 0);
    assert_eq!(auth.refresh_interval(), None);
}
