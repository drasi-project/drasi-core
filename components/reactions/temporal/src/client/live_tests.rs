// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use drasi_lib::{
    channels::{QueryResult, ResultDiff},
    component_graph::ComponentGraph,
    context::ReactionRuntimeContext,
    ComponentStatus, MemoryStateStoreProvider, Reaction, ReactionCheckpoint, StateStoreProvider,
};
use serde_json::{json, Value};
use temporalio_common::protos::temporal::api::{
    history::v1::history_event::Attributes, query::v1::WorkflowQuery,
};
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::{
    Runtime, SyncWorkflowContext, Worker, WorkerOptions, WorkflowContext, WorkflowContextView,
    WorkflowResult,
};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
    GenericImage, ImageExt,
};

use super::*;
use crate::TemporalReaction;

#[workflow]
struct BoundaryWorkflow {
    initial: Value,
    signals: Vec<Value>,
    updates: Vec<Value>,
}

#[workflow_methods]
impl BoundaryWorkflow {
    #[init]
    pub fn new(_ctx: &WorkflowContextView, initial: Value) -> Self {
        Self {
            initial,
            signals: Vec::new(),
            updates: Vec::new(),
        }
    }

    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>) -> WorkflowResult<()> {
        ctx.wait_condition(|_| false).await?;
        Ok(())
    }

    #[signal]
    pub fn notify(&mut self, _ctx: &mut SyncWorkflowContext<Self>, input: Value) {
        self.signals.push(input);
    }

    #[update_validator(change)]
    fn validate_change(
        &self,
        _ctx: &WorkflowContextView,
        input: &Value,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if input["reject"] == true {
            Err("test update rejected".into())
        } else {
            Ok(())
        }
    }

    #[update]
    pub fn change(&mut self, _ctx: &mut SyncWorkflowContext<Self>, input: Value) -> Value {
        self.updates.push(input.clone());
        input
    }

    #[query]
    pub fn snapshot(&self, _ctx: &WorkflowContextView) -> Value {
        json!({"initial":self.initial, "signals":self.signals, "updates":self.updates})
    }
}

fn command(action: Value, payload: Option<Value>) -> Command {
    Command {
        request_id: uuid::Uuid::new_v4().to_string(),
        action: serde_json::from_value(action).unwrap(),
        payload,
        resolved_run_id: None,
    }
}

fn start(id: &str) -> Value {
    json!({"action":"start", "workflowId":id, "workflowType":"BoundaryWorkflow", "taskQueue":"boundary"})
}

async fn dispatch(config: &TemporalReactionConfig, action: Value, row: Value) -> Result<Command> {
    let mut config = config.clone();
    let mut action = action;
    if action["action"] != "cancel" {
        action["template"] = json!("{{json after}}");
    }
    config.output_templates = serde_json::from_value(json!({"routes":{"q":{"added":action}}}))?;
    let store = Arc::new(MemoryStateStoreProvider::new());
    let id = uuid::Uuid::new_v4().to_string();
    let reaction = TemporalReaction::builder(&id)
        .with_query("q")
        .with_config(config)
        .build()?;
    let (graph, _rx) = ComponentGraph::new("live-test");
    reaction
        .initialize(ReactionRuntimeContext::new(
            "live-test",
            &id,
            Some(store.clone()),
            graph.update_sender(),
            None,
        ))
        .await;
    reaction
        .base
        .write_checkpoint(
            "q",
            &ReactionCheckpoint {
                sequence: 0,
                config_hash: 42,
            },
        )
        .await?;
    reaction.start().await?;
    let result = async {
        reaction
            .enqueue_query_result(QueryResult {
                query_id: "q".into(),
                sequence: 1,
                timestamp: chrono::Utc::now(),
                results: vec![ResultDiff::Add {
                    data: row,
                    row_signature: 1,
                }],
                metadata: Default::default(),
                profiling: None,
            })
            .await?;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let status = reaction.status().await;
                ensure!(
                    status != ComponentStatus::Error,
                    "live reaction failed before checkpoint"
                );
                if reaction
                    .base
                    .read_checkpoint("q")
                    .await?
                    .context("checkpoint missing")?
                    .sequence
                    == 1
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("live reaction did not checkpoint within 20 seconds")??;
        let bytes = store
            .get(&id, "temporal:pending:q")
            .await?
            .context("plan missing")?;
        let plan: Value = serde_json::from_slice(&bytes)?;
        Ok(serde_json::from_value(plan["commands"][0].clone())?)
    }
    .await;
    reaction.stop().await?;
    result
}

async fn snapshot(
    client: &Client,
    config: &TemporalReactionConfig,
    target: &Command,
) -> Result<Value> {
    let response = client
        .clone()
        .query_workflow(Request::new(QueryWorkflowRequest {
            namespace: config.namespace.clone(),
            execution: Some(execution(target)),
            query: Some(WorkflowQuery {
                query_type: "snapshot".into(),
                ..Default::default()
            }),
            ..Default::default()
        }))
        .await?
        .into_inner();
    let payload = response
        .query_result
        .context("missing query result")?
        .payloads;
    ensure!(payload.len() == 1, "expected one JSON snapshot");
    ensure!(
        payload[0].metadata["encoding"] == b"json/plain",
        "wrong query encoding"
    );
    Ok(serde_json::from_slice(&payload[0].data)?)
}

async fn wait_status(
    client: &Client,
    config: &TemporalReactionConfig,
    target: &Command,
    expected: WorkflowExecutionStatus,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if describe(client, config, target).await?.status == expected as i32 {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .with_context(|| format!("workflow did not reach {expected:?} within 15 seconds"))?
}

fn assert_code(error: anyhow::Error, code: temporalio_client::tonic::Code) -> Result<()> {
    ensure!(
        error
            .downcast_ref::<temporalio_client::tonic::Status>()
            .is_some_and(|s| s.code() == code),
        "expected {code:?}, got {error:#}"
    );
    Ok(())
}

async fn exercise_actions(client: &Client, config: &TemporalReactionConfig) -> Result<()> {
    let initial = json!({"initial":1});
    let started = dispatch(config, start("actions"), initial.clone()).await?;
    let run = resolve_run(client, config, &started).await?;
    send(client, config, &started).await?;
    ensure!(
        resolve_run(client, config, &started).await? == run,
        "start retry created another run"
    );
    let conflict = command(start("actions"), Some(initial.clone()));
    assert_code(
        send(client, config, &conflict).await.unwrap_err(),
        temporalio_client::tonic::Code::AlreadyExists,
    )?;
    let mut use_existing = start("actions");
    use_existing["conflictPolicy"] = json!("use_existing");
    send(
        client,
        config,
        &command(use_existing, Some(initial.clone())),
    )
    .await?;
    ensure!(
        resolve_run(client, config, &started).await? == run,
        "use_existing replaced the execution"
    );

    let signal_payload = json!({"signal":2});
    let signal = dispatch(
        config,
        json!({"action":"signal", "workflowId":"actions", "signalName":"notify"}),
        signal_payload.clone(),
    )
    .await?;
    send(client, config, &signal).await?;
    let update_payload = json!({"update":3});
    let update = dispatch(
        config,
        json!({"action":"update", "workflowId":"actions", "updateName":"change"}),
        update_payload.clone(),
    )
    .await?;
    send(client, config, &update).await?;
    let accepted_payload = json!({"accepted":4});
    dispatch(config, json!({"action":"update", "workflowId":"actions", "updateName":"change", "waitFor":"accepted"}), accepted_payload.clone()).await?;
    let expected = json!({"initial":initial, "signals":[signal_payload], "updates":[update_payload, accepted_payload]});
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if snapshot(client, config, &signal).await? == expected {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("workflow did not process the exact payloads once")??;

    let rejected = command(
        json!({"action":"update", "workflowId":"actions", "updateName":"change"}),
        Some(json!({"reject":true})),
    );
    let error = send(client, config, &rejected).await.unwrap_err();
    ensure!(
        format!("{error:#}").contains("test update rejected"),
        "update rejection was not propagated: {error:#}"
    );
    let missing = command(
        json!({"action":"signal", "workflowId":"missing", "signalName":"notify"}),
        None,
    );
    assert_code(
        send(client, config, &missing).await.unwrap_err(),
        temporalio_client::tonic::Code::NotFound,
    )?;
    assert_code(
        resolve_run(client, config, &missing).await.unwrap_err(),
        temporalio_client::tonic::Code::NotFound,
    )?;

    let history = client
        .clone()
        .get_workflow_execution_history(Request::new(GetWorkflowExecutionHistoryRequest {
            namespace: config.namespace.clone(),
            execution: Some(execution(&signal)),
            ..Default::default()
        }))
        .await?
        .into_inner()
        .history
        .context("history missing")?;
    ensure!(
        history
            .events
            .iter()
            .filter(|e| matches!(
                e.attributes,
                Some(Attributes::WorkflowExecutionSignaledEventAttributes(_))
            ))
            .count()
            == 1,
        "duplicate signal reached workflow history"
    );
    ensure!(
        history
            .events
            .iter()
            .filter(|e| matches!(
                e.attributes,
                Some(Attributes::WorkflowExecutionUpdateAcceptedEventAttributes(
                    _
                ))
            ))
            .count()
            == 2,
        "update retries were not deduplicated"
    );

    for (action, terminal) in [
        ("cancel", WorkflowExecutionStatus::Canceled),
        ("terminate", WorkflowExecutionStatus::Terminated),
    ] {
        let target = format!("{action}-target");
        let original = dispatch(config, start(&target), json!({})).await?;
        let original_run = resolve_run(client, config, &original).await?;
        let mut destructive = dispatch(
            config,
            json!({"action":action, "workflowId":target, "reason":"test reason"}),
            json!({"details":5}),
        )
        .await?;
        destructive.resolved_run_id = Some(original_run.clone());
        wait_status(client, config, &destructive, terminal).await?;
        let history = client
            .clone()
            .get_workflow_execution_history(Request::new(GetWorkflowExecutionHistoryRequest {
                namespace: config.namespace.clone(),
                execution: Some(execution(&destructive)),
                ..Default::default()
            }))
            .await?
            .into_inner()
            .history
            .context("history missing")?;
        if action == "terminate" {
            let details = history
                .events
                .iter()
                .find_map(|e| match &e.attributes {
                    Some(Attributes::WorkflowExecutionTerminatedEventAttributes(a)) => Some(a),
                    _ => None,
                })
                .context("termination event missing")?;
            ensure!(
                details.reason == "test reason",
                "termination reason changed"
            );
            let payload = &details
                .details
                .as_ref()
                .context("termination payload missing")?
                .payloads[0];
            ensure!(
                payload.metadata["encoding"] == b"json/plain",
                "termination details encoding changed"
            );
            ensure!(
                serde_json::from_slice::<Value>(&payload.data)? == json!({"details":5}),
                "termination details changed"
            );
        } else {
            ensure!(history.events.iter().any(|e| matches!(&e.attributes,
                Some(Attributes::WorkflowExecutionCancelRequestedEventAttributes(a)) if a.cause == "test reason")),
                "cancellation reason missing");
        }
        assert_code(
            send(client, config, &command(start(&target), Some(json!({}))))
                .await
                .unwrap_err(),
            temporalio_client::tonic::Code::AlreadyExists,
        )?;
        let opposite = if action == "cancel" {
            "terminate"
        } else {
            "cancel"
        };
        let closed = command(
            json!({
                "action":opposite, "workflowId":target, "runId":original_run, "reason":"wrong terminal state"
            }),
            None,
        );
        let error = send(client, config, &closed).await.unwrap_err();
        ensure!(
            format!("{error:#}").contains("target execution has already closed"),
            "wrong terminal state was treated as success: {error:#}"
        );
        let mut replacement = start(&target);
        replacement["reusePolicy"] = json!("allow_duplicate");
        let replacement = dispatch(config, replacement, json!({})).await?;
        ensure!(
            resolve_run(client, config, &replacement).await? != original_run,
            "replacement run did not start"
        );
        send(client, config, &destructive).await?;
        ensure!(
            describe(client, config, &replacement).await?.status
                == WorkflowExecutionStatus::Running as i32,
            "destructive retry affected the replacement execution"
        );
    }
    Ok(())
}

#[tokio::test]
async fn real_server_actions_and_error_paths() -> Result<()> {
    let server = GenericImage::new("temporalio/temporal", "1.9.1")
        .with_exposed_port(7233.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Server:"))
        .with_cmd(["server", "start-dev", "--ip", "0.0.0.0", "--headless"])
        .with_startup_timeout(Duration::from_secs(60))
        .start()
        .await?;
    let config = TemporalReactionConfig {
        endpoint: format!(
            "http://{}:{}",
            server.get_host().await?,
            server.get_host_port_ipv4(7233).await?
        ),
        timeout_ms: 15_000,
        ..Default::default()
    };
    let client = connect(&config, "live-test").await?;
    let runtime = Runtime::from_current_tokio(Default::default())?;
    let options = WorkerOptions::new("boundary")
        .register_workflow::<BoundaryWorkflow>()?
        .build();
    let mut worker = Worker::new(&runtime, client.clone(), options)?;
    let shutdown = worker.shutdown_handle();
    let (worker_result, test_result) = tokio::time::timeout(Duration::from_secs(120), async {
        tokio::join!(worker.run(), async {
            let result =
                tokio::time::timeout(Duration::from_secs(90), exercise_actions(&client, &config))
                    .await;
            shutdown();
            result
        })
    })
    .await
    .context("Temporal test/worker shutdown exceeded 120 seconds")?;
    server.rm().await?;
    worker_result?;
    test_result.context("Temporal action scenarios exceeded 90 seconds")?
}
