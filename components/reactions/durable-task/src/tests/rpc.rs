// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use drasi_lib::{
    component_graph::ComponentGraph,
    context::ReactionRuntimeContext,
    identity::{ApplicationIdentityProvider, Credentials},
    ComponentStatus, ReactionCheckpoint, ReactionRecoveryPolicy,
};
use drasi_state_store_redb::RedbStateStoreProvider;
use tokio::{net::TcpListener, sync::Mutex};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status};

use super::*;
use crate::{
    client::DtsClient,
    proto::{
        self,
        task_hub_sidecar_service_server::{TaskHubSidecarService, TaskHubSidecarServiceServer},
    },
};

#[derive(Default)]
struct Scheduler {
    attempts: AtomicUsize,
    states: Mutex<HashMap<String, proto::OrchestrationState>>,
    starts: Mutex<Vec<proto::CreateInstanceRequest>>,
    events: Mutex<Vec<proto::RaiseEventRequest>>,
    terminations: Mutex<Vec<proto::TerminateRequest>>,
    lookups: Mutex<Vec<proto::GetInstanceRequest>>,
    lose_start_response: AtomicBool,
    lose_event_response: AtomicBool,
    unavailable: AtomicBool,
}

impl Scheduler {
    fn metadata<T>(&self, request: &Request<T>) -> Result<(), Status> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.metadata().get("taskhub").unwrap(), "default");
        assert!(request.metadata().get("x-user-agent").is_some());
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(Status::unavailable("injected outage"));
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl TaskHubSidecarService for Arc<Scheduler> {
    async fn start_instance(
        &self,
        request: Request<proto::CreateInstanceRequest>,
    ) -> Result<Response<proto::CreateInstanceResponse>, Status> {
        self.metadata(&request)?;
        let request = request.into_inner();
        assert_eq!(
            request
                .orchestration_id_reuse_policy
                .as_ref()
                .unwrap()
                .replaceable_status,
            Vec::<i32>::new()
        );
        self.starts.lock().await.push(request.clone());
        let mut states = self.states.lock().await;
        if states.contains_key(&request.instance_id) {
            return Err(Status::already_exists("existing instance"));
        }
        states.insert(
            request.instance_id.clone(),
            proto::OrchestrationState {
                instance_id: request.instance_id.clone(),
                name: request.name,
                version: request.version,
                input: request.input,
                tags: request.tags,
                orchestration_status: proto::OrchestrationStatus::Running.into(),
                ..Default::default()
            },
        );
        if self.lose_start_response.swap(false, Ordering::SeqCst) {
            return Err(Status::unavailable("accepted, response lost"));
        }
        Ok(Response::new(proto::CreateInstanceResponse {
            instance_id: request.instance_id,
        }))
    }

    async fn get_instance(
        &self,
        request: Request<proto::GetInstanceRequest>,
    ) -> Result<Response<proto::GetInstanceResponse>, Status> {
        self.metadata(&request)?;
        let request = request.into_inner();
        self.lookups.lock().await.push(request.clone());
        let state = self.states.lock().await.get(&request.instance_id).cloned();
        Ok(Response::new(proto::GetInstanceResponse {
            exists: state.is_some(),
            orchestration_state: state,
        }))
    }

    async fn raise_event(
        &self,
        request: Request<proto::RaiseEventRequest>,
    ) -> Result<Response<proto::RaiseEventResponse>, Status> {
        self.metadata(&request)?;
        self.events.lock().await.push(request.into_inner());
        if self.lose_event_response.swap(false, Ordering::SeqCst) {
            return Err(Status::deadline_exceeded("accepted, response lost"));
        }
        Ok(Response::new(proto::RaiseEventResponse {}))
    }

    async fn terminate_instance(
        &self,
        request: Request<proto::TerminateRequest>,
    ) -> Result<Response<proto::TerminateResponse>, Status> {
        self.metadata(&request)?;
        let request = request.into_inner();
        self.terminations.lock().await.push(request.clone());
        self.states
            .lock()
            .await
            .get_mut(&request.instance_id)
            .unwrap()
            .orchestration_status = proto::OrchestrationStatus::Terminated.into();
        Ok(Response::new(proto::TerminateResponse {}))
    }
}

struct Server {
    state: Arc<Scheduler>,
    endpoint: String,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn new() -> Self {
        let state = Arc::new(Scheduler::default());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let service = TaskHubSidecarServiceServer::new(state.clone());
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        Self {
            state,
            endpoint,
            task,
        }
    }

    fn config(&self) -> DurableTaskReactionConfig {
        DurableTaskReactionConfig {
            endpoint: self.endpoint.clone(),
            ..config()
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn initialize(reaction: &DurableTaskReaction, store: Arc<RedbStateStoreProvider>) {
    let (graph, _) = ComponentGraph::new("dts-test");
    reaction
        .initialize(ReactionRuntimeContext::new(
            "dts-test",
            "r",
            Some(store),
            graph.update_sender(),
            None,
        ))
        .await;
}

async fn checkpoint(reaction: &DurableTaskReaction, sequence: u64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert_ne!(reaction.status().await, ComponentStatus::Error);
            if reaction
                .base
                .read_checkpoint("orders")
                .await
                .unwrap()
                .is_some_and(|cp| cp.sequence == sequence)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("checkpoint timeout");
}

#[tokio::test]
async fn accepted_start_with_lost_response_and_persistent_restart_is_not_replaced() {
    let server = Server::new().await;
    server
        .state
        .lose_start_response
        .store(true, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.redb");
    let store = Arc::new(RedbStateStoreProvider::new(&path).unwrap());
    let reaction = DurableTaskReaction::new("r", vec!["orders".into()], server.config()).unwrap();
    initialize(&reaction, store.clone()).await;
    reaction.start().await.unwrap();
    assert!(reaction.start().await.is_err());
    assert_eq!(reaction.status().await, ComponentStatus::Running);
    let result = emission(1, vec![add("a")]);
    reaction.enqueue_query_result(result.clone()).await.unwrap();
    checkpoint(&reaction, 1).await;
    assert_eq!(server.state.starts.lock().await.len(), 2);
    assert_eq!(server.state.states.lock().await.len(), 1);
    reaction.stop().await.unwrap();
    reaction.stop().await.unwrap();
    drop(reaction);
    drop(store);

    let store = Arc::new(RedbStateStoreProvider::new(&path).unwrap());
    let reconstructed =
        DurableTaskReaction::new("r", vec!["orders".into()], server.config()).unwrap();
    initialize(&reconstructed, store).await;
    reconstructed.start().await.unwrap();
    reconstructed.enqueue_query_result(result).await.unwrap();
    reconstructed
        .enqueue_query_result(emission(2, vec![]))
        .await
        .unwrap();
    checkpoint(&reconstructed, 2).await;
    assert_eq!(
        server.state.starts.lock().await.len(),
        2,
        "checkpointed emission must not be resent"
    );
    reconstructed.stop().await.unwrap();
}

#[tokio::test]
async fn duplicates_reconcile_terminal_instances_and_conflicts_obey_configuration() {
    let server = Server::new().await;
    let mut client = DtsClient::new(&server.config(), None).unwrap();
    let Dispatch::Start { instance_id, .. } = &prepare(
        &server.config(),
        "r",
        &emission(1, vec![add("a")]),
        &handlebars(),
    )
    .unwrap()[0] else {
        panic!("expected start")
    };
    let operation = Dispatch::Start {
        instance_id: instance_id.clone(),
        orchestration: "Workflow".into(),
        version: String::new(),
        input: "{\"x\":1}".into(),
        command_id: "command-1".into(),
        on_conflict: TargetPolicy::Error,
    };
    client.dispatch(&operation).await.unwrap();
    server
        .state
        .states
        .lock()
        .await
        .get_mut(instance_id)
        .unwrap()
        .orchestration_status = proto::OrchestrationStatus::Completed.into();
    client.dispatch(&operation).await.unwrap();
    let mut conflict = operation.clone();
    if let Dispatch::Start { input, .. } = &mut conflict {
        *input = "{\"x\":2}".into();
    }
    assert!(client.dispatch(&conflict).await.is_err());
    if let Dispatch::Start { on_conflict, .. } = &mut conflict {
        *on_conflict = TargetPolicy::Ignore;
    }
    client.dispatch(&conflict).await.unwrap();
    assert_eq!(
        server.state.states.lock().await[instance_id]
            .input
            .as_deref(),
        Some("{\"x\":1}")
    );
}

#[tokio::test]
async fn events_retain_identity_on_retry_and_missing_terminal_policies_are_independent() {
    let server = Server::new().await;
    server
        .state
        .states
        .lock()
        .await
        .insert("chosen".into(), proto::OrchestrationState::default());
    let mut client = DtsClient::new(&server.config(), None).unwrap();
    server
        .state
        .lose_event_response
        .store(true, Ordering::SeqCst);
    let mut operation = Dispatch::Event {
        instance_id: "chosen".into(),
        event_name: "approved".into(),
        input: "{\"commandId\":\"stable\"}".into(),
        on_missing: TargetPolicy::Error,
        on_terminal: TargetPolicy::Error,
    };
    client.dispatch(&operation).await.unwrap();
    let events = server.state.events.lock().await.clone();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0], events[1],
        "retry must not invent a new command ID"
    );
    server
        .state
        .states
        .lock()
        .await
        .get_mut("chosen")
        .unwrap()
        .orchestration_status = proto::OrchestrationStatus::Completed.into();
    assert!(client.dispatch(&operation).await.is_err());
    if let Dispatch::Event { on_terminal, .. } = &mut operation {
        *on_terminal = TargetPolicy::Ignore;
    }
    client.dispatch(&operation).await.unwrap();
    server.state.states.lock().await.clear();
    assert!(
        client.dispatch(&operation).await.is_err(),
        "onTerminal must not change onMissing"
    );
    if let Dispatch::Event { on_missing, .. } = &mut operation {
        *on_missing = TargetPolicy::Ignore;
    }
    client.dispatch(&operation).await.unwrap();
    assert_eq!(
        server.state.events.lock().await.len(),
        2,
        "ignored targets must not receive an RPC"
    );
}

#[tokio::test]
async fn terminate_and_ignore_are_operator_selected_not_inferred_from_delete() {
    let server = Server::new().await;
    server
        .state
        .states
        .lock()
        .await
        .insert("entity-7".into(), proto::OrchestrationState::default());
    let mut c = server.config();
    c.default_template = Some(DurableTaskQueryConfig {
        added: Some(DurableTaskTemplate::with_extension("", Action::Ignore)),
        updated: Some(DurableTaskTemplate::with_extension(
            "",
            Action::Terminate {
                instance_id: "entity-{{after.id}}".into(),
                recursive: true,
                on_missing: TargetPolicy::Error,
            },
        )),
        deleted: Some(start(None)),
    });
    let mut client = DtsClient::new(&c, None).unwrap();
    let events = emission(
        5,
        vec![
            add("ignored"),
            ResultDiff::Update {
                data: json!({}),
                before: json!({"id":7}),
                after: json!({"id":7}),
                grouping_keys: None,
                row_signature: 7,
            },
            ResultDiff::Delete {
                data: json!({"id":7}),
                row_signature: 7,
            },
        ],
    );
    for op in prepare(&c, "r", &events, &handlebars()).unwrap() {
        client.dispatch(&op).await.unwrap();
    }
    assert_eq!(server.state.terminations.lock().await.len(), 1);
    assert!(server.state.terminations.lock().await[0].recursive);
    assert_eq!(
        server.state.starts.lock().await.len(),
        1,
        "DELETE explicitly starts a workflow"
    );
    assert_eq!(
        server.state.starts.lock().await[0]
            .input
            .as_ref()
            .map(|s| serde_json::from_str::<Value>(s).unwrap()["operation"].clone()),
        Some(json!("DELETE"))
    );
}

#[tokio::test]
async fn failure_does_not_checkpoint_and_restart_replays_partial_batch_safely() {
    let server = Server::new().await;
    server.state.states.lock().await.insert(
        "b".into(),
        proto::OrchestrationState {
            name: "OtherWorkflow".into(),
            ..Default::default()
        },
    );
    let mut c = server.config();
    c.default_template.as_mut().unwrap().added = Some(start(Some("{{after.id}}")));
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(RedbStateStoreProvider::new(dir.path().join("state.redb")).unwrap());
    let reaction = DurableTaskReaction::new("r", vec!["orders".into()], c).unwrap();
    initialize(&reaction, store).await;
    reaction.start().await.unwrap();
    let result = emission(1, vec![add("a"), add("b")]);
    reaction.enqueue_query_result(result.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while reaction.status().await != ComponentStatus::Error {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(reaction
        .base
        .read_checkpoint("orders")
        .await
        .unwrap()
        .is_none());
    assert!(
        server.state.states.lock().await.contains_key("a"),
        "first effect was accepted"
    );
    reaction.stop().await.unwrap();
    server.state.states.lock().await.remove("b");
    reaction.start().await.unwrap();
    reaction.enqueue_query_result(result).await.unwrap();
    checkpoint(&reaction, 1).await;
    assert_eq!(server.state.states.lock().await.len(), 2);
    reaction.stop().await.unwrap();
}

#[tokio::test]
async fn authentication_refreshes_tokens_and_marks_metadata_sensitive() {
    let counter = Arc::new(AtomicUsize::new(0));
    let calls = counter.clone();
    let provider = Arc::new(ApplicationIdentityProvider::new_sync(move |context| {
        assert_eq!(context.get("scope"), Some(crate::config::DTS_SCOPE));
        Ok(Credentials::Token {
            username: String::new(),
            token: format!("token-{}", calls.fetch_add(1, Ordering::SeqCst)),
        })
    }));
    let c = DurableTaskReactionConfig {
        endpoint: "https://scheduler.example.com".into(),
        authentication: Authentication::Identity,
        ..config()
    };
    assert!(DtsClient::new(&c, None).is_err());
    let client = DtsClient::new(&c, Some(provider)).unwrap();
    let first = client.request(()).await.unwrap();
    let second = client.request(()).await.unwrap();
    assert_eq!(
        first.metadata().get("authorization").unwrap(),
        "Bearer token-0"
    );
    assert_eq!(
        second.metadata().get("authorization").unwrap(),
        "Bearer token-1"
    );
    assert!(first
        .metadata()
        .get("authorization")
        .unwrap()
        .is_sensitive());
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn durable_reaction_rejects_volatile_storage_and_stop_before_start_is_safe() {
    let reaction = DurableTaskReaction::new("r", vec!["orders".into()], config()).unwrap();
    reaction.stop().await.unwrap();
    assert!(reaction.start().await.is_err());
    assert_eq!(reaction.status().await, ComponentStatus::Error);
    reaction.stop().await.unwrap();
}

#[tokio::test]
async fn retry_exhaustion_stops_without_advancing_checkpoint() {
    let server = Server::new().await;
    server.state.unavailable.store(true, Ordering::SeqCst);
    let mut config = server.config();
    config.max_retries = 2;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(RedbStateStoreProvider::new(dir.path().join("state.redb")).unwrap());
    let reaction = DurableTaskReaction::new("r", vec!["orders".into()], config).unwrap();
    initialize(&reaction, store).await;
    reaction.start().await.unwrap();
    reaction
        .enqueue_query_result(emission(1, vec![add("a")]))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while reaction.status().await != ComponentStatus::Error {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(server.state.attempts.load(Ordering::SeqCst), 3);
    assert!(reaction
        .base
        .read_checkpoint("orders")
        .await
        .unwrap()
        .is_none());
    reaction.stop().await.unwrap();
}

#[tokio::test]
async fn invalid_auth_credentials_fail_without_constructing_authorization() {
    for credentials in [
        Credentials::Token {
            username: String::new(),
            token: String::new(),
        },
        Credentials::Token {
            username: String::new(),
            token: "line\nbreak".into(),
        },
        Credentials::UsernamePassword {
            username: "not-token".into(),
            password: "not-used".into(),
        },
    ] {
        let provider = Arc::new(ApplicationIdentityProvider::new_sync(move |_| {
            Ok(credentials.clone())
        }));
        let c = DurableTaskReactionConfig {
            endpoint: "https://scheduler.example.com".into(),
            authentication: Authentication::Identity,
            ..config()
        };
        assert!(DtsClient::new(&c, Some(provider))
            .unwrap()
            .request(())
            .await
            .is_err());
    }
}

#[tokio::test]
async fn target_checks_do_not_fetch_workflow_payloads() {
    let server = Server::new().await;
    let mut client = DtsClient::new(&server.config(), None).unwrap();
    server
        .state
        .states
        .lock()
        .await
        .insert("target".into(), proto::OrchestrationState::default());
    client
        .dispatch(&Dispatch::Event {
            instance_id: "target".into(),
            event_name: "approved".into(),
            input: "{}".into(),
            on_missing: TargetPolicy::Error,
            on_terminal: TargetPolicy::Error,
        })
        .await
        .unwrap();
    client
        .dispatch(&Dispatch::Terminate {
            instance_id: "target".into(),
            output: "\"done\"".into(),
            recursive: false,
            on_missing: TargetPolicy::Error,
        })
        .await
        .unwrap();
    let lookups = server.state.lookups.lock().await;
    assert_eq!(lookups.len(), 2);
    assert!(lookups
        .iter()
        .all(|request| !request.get_inputs_and_outputs));
}

#[tokio::test]
async fn start_replay_survives_continue_as_new_without_accepting_changed_commands() {
    let server = Server::new().await;
    let mut client = DtsClient::new(&server.config(), None).unwrap();
    let operation = Dispatch::Start {
        instance_id: "continued".into(),
        orchestration: "Workflow".into(),
        version: "1".into(),
        input: "{\"page\":1}".into(),
        command_id: "original-command".into(),
        on_conflict: TargetPolicy::Error,
    };
    client.dispatch(&operation).await.unwrap();
    {
        let mut states = server.state.states.lock().await;
        let state = states.get_mut("continued").unwrap();
        state.version = Some("2".into());
        state.input = Some("{\"page\":2}".into());
    }
    client.dispatch(&operation).await.unwrap();
    for changed in ["name", "version", "input", "command"] {
        let mut conflict = operation.clone();
        if let Dispatch::Start {
            orchestration,
            version,
            input,
            command_id,
            ..
        } = &mut conflict
        {
            match changed {
                "name" => *orchestration = "OtherWorkflow".into(),
                "version" => *version = "2".into(),
                "input" => *input = "{\"page\":2}".into(),
                "command" => *command_id = "different-command".into(),
                _ => unreachable!(),
            }
        }
        assert!(client.dispatch(&conflict).await.is_err(), "{changed}");
    }
    assert_eq!(
        server.state.states.lock().await["continued"]
            .input
            .as_deref(),
        Some("{\"page\":2}"),
        "reconciliation must not replace the continued execution"
    );
    assert!(server
        .state
        .lookups
        .lock()
        .await
        .iter()
        .all(|request| !request.get_inputs_and_outputs));
    server
        .state
        .states
        .lock()
        .await
        .get_mut("continued")
        .unwrap()
        .tags
        .remove("drasi-start-fingerprint");
    assert!(
        client.dispatch(&operation).await.is_err(),
        "missing original-start evidence must not be accepted"
    );
}

#[tokio::test]
async fn running_publication_has_a_task_handle_and_gates_queued_work() {
    let server = Server::new().await;
    server.state.unavailable.store(true, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(RedbStateStoreProvider::new(dir.path().join("state.redb")).unwrap());
    let mut config = server.config();
    config.max_retries = 0;
    let reaction = Arc::new(DurableTaskReaction::new("r", vec!["orders".into()], config).unwrap());
    let (updates, receiver) = tokio::sync::mpsc::channel(1);
    reaction
        .initialize(ReactionRuntimeContext::new(
            "dts-test",
            "r",
            Some(store),
            updates,
            None,
        ))
        .await;
    reaction
        .enqueue_query_result(emission(1, vec![add("a")]))
        .await
        .unwrap();
    let mut status = reaction.base.status_handle().subscribe_status();
    let starting = tokio::spawn({
        let reaction = reaction.clone();
        async move { reaction.start().await }
    });
    tokio::time::timeout(
        Duration::from_secs(5),
        status.wait_for(|status| *status == ComponentStatus::Running),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        reaction.base.processing_task.read().await.is_some(),
        "Running must not be visible before its task handle is installed"
    );
    assert_eq!(
        server.state.attempts.load(Ordering::SeqCst),
        0,
        "queued work must wait until Running publication finishes"
    );
    drop(receiver);
    starting.await.unwrap().unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        status.wait_for(|status| *status == ComponentStatus::Error),
    )
    .await
    .unwrap()
    .unwrap();
    reaction.stop().await.unwrap();
}

#[tokio::test]
async fn recovery_uses_the_hosts_new_query_epoch_instead_of_the_startup_snapshot() {
    let server = Server::new().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(RedbStateStoreProvider::new(dir.path().join("state.redb")).unwrap());
    let reaction = DurableTaskReaction::builder("r")
        .with_queries(vec!["orders".into()])
        .with_config(server.config())
        .with_recovery_policy(ReactionRecoveryPolicy::AutoSkipGap)
        .build()
        .unwrap();
    initialize(&reaction, store).await;
    reaction
        .base
        .write_checkpoint(
            "orders",
            &ReactionCheckpoint {
                sequence: 99,
                config_hash: 1,
            },
        )
        .await
        .unwrap();
    reaction.start().await.unwrap();
    reaction
        .base
        .write_checkpoint(
            "orders",
            &ReactionCheckpoint {
                sequence: 3,
                config_hash: 2,
            },
        )
        .await
        .unwrap();
    reaction
        .enqueue_query_result(emission(4, vec![add("new-epoch")]))
        .await
        .unwrap();
    checkpoint(&reaction, 4).await;
    assert_eq!(
        reaction
            .base
            .read_checkpoint("orders")
            .await
            .unwrap()
            .unwrap()
            .config_hash,
        2
    );
    assert_eq!(server.state.starts.lock().await.len(), 1);
    reaction.stop().await.unwrap();
}

#[tokio::test]
async fn live_recovery_checkpoint_skips_buffered_emissions_without_regressing() {
    let server = Server::new().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(RedbStateStoreProvider::new(dir.path().join("state.redb")).unwrap());
    let reaction = DurableTaskReaction::new("r", vec!["orders".into()], server.config()).unwrap();
    initialize(&reaction, store).await;
    reaction.start().await.unwrap();
    reaction
        .enqueue_query_result(emission(1, vec![add("initial")]))
        .await
        .unwrap();
    checkpoint(&reaction, 1).await;
    reaction
        .base
        .write_checkpoint(
            "orders",
            &ReactionCheckpoint {
                sequence: 10,
                config_hash: 7,
            },
        )
        .await
        .unwrap();
    reaction
        .enqueue_query_result(emission(2, vec![add("skipped")]))
        .await
        .unwrap();
    reaction
        .enqueue_query_result(emission(11, vec![add("latest")]))
        .await
        .unwrap();
    checkpoint(&reaction, 11).await;
    assert_eq!(
        server.state.starts.lock().await.len(),
        2,
        "host-skipped emissions must not be delivered"
    );
    assert_eq!(
        reaction
            .base
            .read_checkpoint("orders")
            .await
            .unwrap()
            .unwrap()
            .config_hash,
        7
    );
    reaction.stop().await.unwrap();
}
