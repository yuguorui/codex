use super::*;
use crate::agent::control::SpawnAgentOptions;
use crate::config::test_config;
use crate::init_state_db;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::user_input::UserInput;
use core_test_support::PathBufExt;
use core_test_support::PathExt;
use std::time::Duration;
use tempfile::tempdir;

const TEST_INSTALLATION_ID: &str = "11111111-1111-4111-8111-111111111111";

#[tokio::test]
async fn rollout_independent_fresh_subagents_use_the_owning_registry() {
    let temp_dir = tempdir().expect("tempdir");
    let mut config = test_config().await;
    config.codex_home = temp_dir.path().join("codex-home").abs();
    config.cwd = config.codex_home.abs();
    config.agent_max_threads = Some(1);
    std::fs::create_dir_all(&config.codex_home).expect("create codex home");
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let first_owner = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start first owner");
    let second_owner = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start second owner");
    let first_control = first_owner.thread.session.services.agent_control.clone();
    let second_control = second_owner.thread.session.services.agent_control.clone();

    let first_agent = manager
        .start_fresh_subagent_without_rollout_budget(
            first_owner.thread_id,
            StartThreadOptions {
                session_source: Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: first_owner.thread_id,
                    depth: 1,
                    agent_path: None,
                    agent_nickname: Some("first-workflow-agent".to_string()),
                    agent_role: Some("explorer".to_string()),
                })),
                thread_source: Some(ThreadSource::Subagent),
                ..StartThreadOptions::new(config.clone())
            },
        )
        .await
        .expect("start first workflow agent");
    let concurrent_agent = manager
        .start_fresh_subagent_without_rollout_budget(
            first_owner.thread_id,
            StartThreadOptions {
                session_source: Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: first_owner.thread_id,
                    depth: 1,
                    agent_path: None,
                    agent_nickname: Some("concurrent-workflow-agent".to_string()),
                    agent_role: Some("reviewer".to_string()),
                })),
                thread_source: Some(ThreadSource::Subagent),
                ..StartThreadOptions::new(config.clone())
            },
        )
        .await
        .expect("workflow concurrency should not use the agent thread limit");
    let second_owner_agent = manager
        .start_fresh_subagent_without_rollout_budget(
            second_owner.thread_id,
            StartThreadOptions {
                session_source: Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: second_owner.thread_id,
                    depth: 1,
                    agent_path: None,
                    agent_nickname: Some("second-owner-agent".to_string()),
                    agent_role: Some("worker".to_string()),
                })),
                thread_source: Some(ThreadSource::Subagent),
                ..StartThreadOptions::new(config.clone())
            },
        )
        .await
        .expect("start second owner's workflow agent");
    let ordinary_agent = first_control
        .spawn_agent_with_metadata(
            config,
            vec![UserInput::Text {
                text: "ordinary subagent task".to_string(),
                text_elements: Vec::new(),
            }],
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: first_owner.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                parent_thread_id: Some(first_owner.thread_id),
                ..Default::default()
            },
        )
        .await
        .expect("a live workflow agent should not consume ordinary subagent capacity");

    assert_eq!(
        first_control
            .get_agent_metadata(first_agent.thread_id)
            .map(|metadata| {
                (
                    metadata.agent_id,
                    metadata.agent_path,
                    metadata.agent_nickname,
                    metadata.agent_role,
                )
            }),
        Some((
            Some(first_agent.thread_id),
            None,
            Some("first-workflow-agent".to_string()),
            Some("explorer".to_string()),
        ))
    );
    assert_eq!(
        first_agent
            .thread
            .session
            .services
            .agent_control
            .get_agent_metadata(concurrent_agent.thread_id),
        first_control.get_agent_metadata(concurrent_agent.thread_id),
        "the child and owner controls should share one registry"
    );
    let first_agent_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: first_owner.thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    let _parent_capacity = first_control
        .execution_guard(MultiAgentVersion::V2, &first_agent_source)
        .expect("parent capacity should be limited");
    first_agent
        .thread
        .session
        .services
        .agent_control
        .ensure_execution_capacity(MultiAgentVersion::V2, &first_agent_source)
        .expect("workflow agents should retain independent execution capacity");
    assert_eq!(
        first_control.get_agent_metadata(second_owner_agent.thread_id),
        None
    );
    assert_eq!(
        second_control.get_agent_metadata(first_agent.thread_id),
        None
    );
    assert_eq!(
        second_control
            .get_agent_metadata(second_owner_agent.thread_id)
            .and_then(|metadata| metadata.agent_nickname),
        Some("second-owner-agent".to_string())
    );
    let first_owner_agent_names = first_control
        .list_agents(&SessionSource::Exec, /*path_prefix*/ None)
        .await
        .expect("list first owner's agents")
        .into_iter()
        .map(|agent| agent.agent_name)
        .collect::<Vec<_>>();
    assert!(first_owner_agent_names.contains(&first_agent.thread_id.to_string()));
    assert!(first_owner_agent_names.contains(&concurrent_agent.thread_id.to_string()));
    assert!(first_owner_agent_names.contains(&ordinary_agent.thread_id.to_string()));
    assert!(!first_owner_agent_names.contains(&second_owner_agent.thread_id.to_string()));

    assert_eq!(
        first_agent.thread.force_close(Duration::from_secs(5)).await,
        crate::codex_thread::ThreadTeardownStatus::Confirmed
    );
    first_control
        .close_agent(first_owner.thread_id, first_agent.thread_id)
        .await
        .expect("close should clean up a completed workflow agent");
    first_control
        .close_agent(first_owner.thread_id, first_agent.thread_id)
        .await
        .expect("closing a cleaned-up workflow agent should be idempotent");
    assert_eq!(
        first_control.get_status(first_agent.thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        first_control.get_agent_metadata(first_agent.thread_id),
        None
    );
    assert!(
        first_control
            .get_agent_metadata(concurrent_agent.thread_id)
            .is_some()
    );
    assert!(
        second_control
            .get_agent_metadata(second_owner_agent.thread_id)
            .is_some()
    );
    let first_owner_agent_names = first_control
        .list_agents(&SessionSource::Exec, /*path_prefix*/ None)
        .await
        .expect("list first owner's remaining agents")
        .into_iter()
        .map(|agent| agent.agent_name)
        .collect::<Vec<_>>();
    assert!(!first_owner_agent_names.contains(&first_agent.thread_id.to_string()));
    assert!(first_owner_agent_names.contains(&concurrent_agent.thread_id.to_string()));

    let report = manager
        .shutdown_all_threads_bounded(Duration::from_secs(10))
        .await;
    assert!(report.timed_out.is_empty());
}

#[tokio::test]
async fn fresh_workflow_subagent_persists_ownership_edge_and_close_status() {
    let temp_dir = tempdir().expect("tempdir");
    let mut config = test_config().await;
    config.codex_home = temp_dir.path().join("codex-home").abs();
    config.cwd = config.codex_home.abs();
    std::fs::create_dir_all(&config.codex_home).expect("create codex home");
    let state_db = init_state_db(&config).await;
    let agent_graph_store = local_agent_graph_store_from_state_db(state_db.as_ref())
        .expect("agent graph store should be available");
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let manager = ThreadManager::new(
        &config,
        auth_manager.clone(),
        build_models_manager(&config, auth_manager),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::passthrough_image_store(),
        thread_store_from_config(&config, state_db),
        Some(Arc::clone(&agent_graph_store)),
        TEST_INSTALLATION_ID.to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let owner = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start workflow owner");
    let agent = manager
        .start_fresh_subagent_without_rollout_budget(
            owner.thread_id,
            StartThreadOptions {
                session_source: Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: owner.thread_id,
                    depth: 1,
                    agent_path: None,
                    agent_nickname: Some("workflow-agent".to_string()),
                    agent_role: Some("worker".to_string()),
                })),
                ..StartThreadOptions::new(config)
            },
        )
        .await
        .expect("start workflow agent");
    let owner_control = owner.thread.session.services.agent_control.clone();
    let agent_turn = agent.thread.session.new_default_turn().await;
    agent
        .thread
        .session
        .send_event(
            agent_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: agent_turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("workflow agent result".to_string()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;

    assert_eq!(
        agent_graph_store
            .list_thread_spawn_children(
                owner.thread_id,
                Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Open),
            )
            .await
            .expect("list open workflow edges"),
        vec![agent.thread_id]
    );

    assert_eq!(
        manager
            .force_close_subagent(agent.thread_id, Duration::from_secs(5))
            .await
            .expect("close workflow agent"),
        crate::codex_thread::ThreadTeardownStatus::Confirmed
    );
    let expected_status = AgentStatus::Completed(Some("workflow agent result".to_string()));
    assert_eq!(
        owner_control.get_status(agent.thread_id).await,
        expected_status
    );
    for _ in 0..2 {
        let status = owner_control
            .subscribe_status(agent.thread_id)
            .await
            .expect("completed workflow agent should remain waitable")
            .borrow()
            .clone();
        assert_eq!(status, expected_status);
    }
    assert_eq!(
        agent_graph_store
            .list_thread_spawn_children(
                owner.thread_id,
                Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed),
            )
            .await
            .expect("list closed workflow edges"),
        vec![agent.thread_id]
    );
}
