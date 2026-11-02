// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, ensure, Context, Result};
use std::{collections::HashMap, time::Duration};
use temporalio_client::{
    grpc::WorkflowService, tonic::Request, Client, ClientOptions, ClientTlsOptions, Connection,
    ConnectionOptions, TlsOptions,
};
use temporalio_common::protos::temporal::api::{
    common::v1::{Payload, Payloads, WorkflowExecution},
    enums::v1::{UpdateWorkflowExecutionLifecycleStage, WorkflowExecutionStatus},
    update::v1::outcome::Value as UpdateOutcome,
    workflow::v1::WorkflowExecutionInfo,
    workflowservice::v1::*,
};

use crate::{config::*, render::Command};

mod requests;
use requests::ActionRequest;

#[cfg(test)]
mod live_tests;

#[async_trait::async_trait]
pub(crate) trait Transport: Send + Sync {
    async fn resolve_run(
        &self,
        config: &TemporalReactionConfig,
        command: &Command,
    ) -> Result<String>;
    async fn send(&self, config: &TemporalReactionConfig, command: &Command) -> Result<()>;
}

#[async_trait::async_trait]
impl Transport for Client {
    async fn resolve_run(
        &self,
        config: &TemporalReactionConfig,
        command: &Command,
    ) -> Result<String> {
        resolve_run(self, config, command).await
    }

    async fn send(&self, config: &TemporalReactionConfig, command: &Command) -> Result<()> {
        send(self, config, command).await
    }
}

pub(crate) async fn connect(config: &TemporalReactionConfig, id: &str) -> Result<Client> {
    let endpoint = temporalio_client::Url::parse(&config.endpoint)?;
    let tls = if endpoint.scheme() == "https" {
        let tls = config.tls.clone().unwrap_or_default();
        Some(
            TlsOptions::builder()
                .maybe_server_root_ca_cert(tls.server_root_ca_cert.map(String::into_bytes))
                .maybe_domain(tls.domain)
                .maybe_client_tls_options(tls.client_identity.map(|identity| {
                    ClientTlsOptions::builder()
                        .client_cert(identity.certificate.into_bytes())
                        .client_private_key(identity.private_key.into_bytes())
                        .build()
                }))
                .build(),
        )
    } else {
        None
    };
    let timeout = Duration::from_millis(config.timeout_ms);
    let connection = tokio::time::timeout(
        timeout,
        Connection::connect(
            ConnectionOptions::new(endpoint)
                .identity(format!("drasi/{id}"))
                .connect_timeout(timeout)
                .maybe_api_key(config.api_key.clone())
                .maybe_tls_options(tls)
                .build(),
        ),
    )
    .await
    .context("Temporal connection deadline elapsed")??;
    Ok(Client::new(
        connection,
        ClientOptions::new(config.namespace.clone()).build(),
    )?)
}

fn execution(command: &Command) -> WorkflowExecution {
    WorkflowExecution {
        workflow_id: command.action.workflow_id().to_owned(),
        run_id: command
            .resolved_run_id
            .as_deref()
            .or(command.action.run_id())
            .unwrap_or_default()
            .to_owned(),
    }
}

async fn describe(
    client: &Client,
    config: &TemporalReactionConfig,
    command: &Command,
) -> Result<WorkflowExecutionInfo> {
    let response = client
        .clone()
        .describe_workflow_execution(Request::new(DescribeWorkflowExecutionRequest {
            namespace: config.namespace.clone(),
            execution: Some(execution(command)),
        }))
        .await?
        .into_inner();
    response
        .workflow_execution_info
        .context("Temporal Describe response is missing execution info")
}

pub(crate) async fn resolve_run(
    client: &Client,
    config: &TemporalReactionConfig,
    command: &Command,
) -> Result<String> {
    let info = tokio::time::timeout(
        Duration::from_millis(config.timeout_ms),
        describe(client, config, command),
    )
    .await
    .context("Temporal Describe deadline elapsed")??;
    let run = info
        .execution
        .context("Temporal Describe response is missing execution")?
        .run_id;
    ensure!(
        !run.is_empty(),
        "Temporal Describe response is missing runId"
    );
    Ok(run)
}

fn payload(command: &Command) -> Result<Option<Payloads>> {
    command
        .payload
        .as_ref()
        .map(|value| {
            Ok(Payloads {
                payloads: vec![Payload {
                    metadata: HashMap::from([("encoding".into(), b"json/plain".to_vec())]),
                    data: serde_json::to_vec(value)?,
                    ..Default::default()
                }],
            })
        })
        .transpose()
}

pub(crate) async fn send(
    client: &Client,
    config: &TemporalReactionConfig,
    command: &Command,
) -> Result<()> {
    tokio::time::timeout(
        Duration::from_millis(config.timeout_ms),
        send_inner(client, config, command),
    )
    .await
    .context("Temporal action deadline elapsed; acceptance may be unknown")?
    .with_context(|| format!("Temporal action failed for request {}", command.request_id))
}

async fn send_inner(
    client: &Client,
    config: &TemporalReactionConfig,
    command: &Command,
) -> Result<()> {
    let mut service = client.clone();
    let request = requests::build(config, command)?;
    if matches!(
        request,
        ActionRequest::Cancel(_) | ActionRequest::Terminate(_)
    ) {
        let terminal = if matches!(request, ActionRequest::Cancel(_)) {
            WorkflowExecutionStatus::Canceled
        } else {
            WorkflowExecutionStatus::Terminated
        } as i32;
        let info = describe(client, config, command).await?;
        if info.status == terminal {
            log::info!(
                "Temporal request {} already reached its requested terminal state",
                command.request_id
            );
            return Ok(());
        }
        ensure!(
            info.status == WorkflowExecutionStatus::Running as i32,
            "target execution has already closed"
        );
    }
    match request {
        ActionRequest::Signal(request) => {
            service
                .signal_workflow_execution(Request::new(request))
                .await?;
        }
        ActionRequest::Start(request) => {
            service
                .start_workflow_execution(Request::new(request))
                .await?;
        }
        ActionRequest::Update(request) => {
            let stage = request
                .wait_policy
                .as_ref()
                .context("Update wait policy is missing")?
                .lifecycle_stage;
            loop {
                let response = service
                    .update_workflow_execution(Request::new(request.clone()))
                    .await?
                    .into_inner();
                if let Some(UpdateOutcome::Failure(failure)) =
                    response.outcome.as_ref().and_then(|o| o.value.as_ref())
                {
                    bail!("Temporal Update rejected or failed: {}", failure.message);
                }
                if response.stage >= stage {
                    if stage == UpdateWorkflowExecutionLifecycleStage::Completed as i32 {
                        ensure!(
                            matches!(
                                response.outcome.and_then(|o| o.value),
                                Some(UpdateOutcome::Success(_))
                            ),
                            "completed Temporal Update has no successful outcome"
                        );
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        ActionRequest::Cancel(request) => {
            service
                .request_cancel_workflow_execution(Request::new(request))
                .await?;
        }
        ActionRequest::Terminate(request) => {
            service
                .terminate_workflow_execution(Request::new(request))
                .await?;
        }
    }
    Ok(())
}
