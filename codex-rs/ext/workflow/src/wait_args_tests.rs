use super::*;
use codex_config::LoaderOverrides;
use codex_core::config::ConfigBuilder;
use pretty_assertions::assert_eq;

async fn config() -> Config {
    ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn timeout_uses_the_configured_wait_window() {
    let config = config().await;
    let default = config.multi_agent_v2.default_wait_timeout_ms;
    let minimum = config.multi_agent_v2.min_wait_timeout_ms;
    let maximum = config.multi_agent_v2.max_wait_timeout_ms;
    assert_eq!(resolve_timeout_ms(&config, None).unwrap(), default);
    assert_eq!(
        resolve_timeout_ms(&config, Some(minimum - 1)).unwrap(),
        minimum
    );
    assert_eq!(resolve_timeout_ms(&config, Some(maximum)).unwrap(), maximum);
    assert!(resolve_timeout_ms(&config, Some(maximum + 1)).is_err());
}

#[test]
fn single_run_id_is_length_bounded() {
    assert_eq!(validate_run_id("wf_abc123"), Ok(()));
    assert_eq!(
        validate_run_id(""),
        Err(format!(
            "provide the workflow run id as 1..={MAX_WAIT_WORKFLOW_ID_BYTES} UTF-8 bytes"
        ))
    );
    assert!(validate_run_id(&"w".repeat(MAX_WAIT_WORKFLOW_ID_BYTES)).is_ok());
    assert!(validate_run_id(&"w".repeat(MAX_WAIT_WORKFLOW_ID_BYTES + 1)).is_err());
}

#[test]
fn run_id_set_is_focused_and_unique() {
    assert_eq!(
        validate_run_ids(&[]),
        Err("provide a focused, non-empty set of runIds; split larger sets across additional WaitWorkflows calls".to_string())
    );
    assert_eq!(
        validate_run_ids(&["wf_1".to_string(), "wf_1".to_string()]),
        Err("provide each workflow run id once in runIds".to_string())
    );
    let oversized = (0..MAX_WAIT_WORKFLOW_ITEMS + 1)
        .map(|index| format!("wf_{index}"))
        .collect::<Vec<_>>();
    assert!(validate_run_ids(&oversized).is_err());
    assert!(validate_run_ids(&oversized[..MAX_WAIT_WORKFLOW_ITEMS]).is_ok());
}
