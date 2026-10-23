// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{bail, ensure, Context, Result};
use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};
use tonic::{
    metadata::{Ascii, MetadataValue},
    transport::{Channel, ClientTlsConfig, Endpoint},
    Code, Request,
};

use crate::{
    config::{Authentication, DurableTaskReactionConfig, TargetPolicy},
    dispatch::Dispatch,
    proto::{self, task_hub_sidecar_service_client::TaskHubSidecarServiceClient},
};

const COMMAND_TAG: &str = "drasi-command-id";

#[derive(Clone)]
pub(crate) struct DtsClient {
    inner: TaskHubSidecarServiceClient<Channel>,
    hub: MetadataValue<Ascii>,
    provider: Option<Arc<dyn IdentityProvider>>,
    scope: String,
    timeout: Duration,
    max_retries: u32,
    retry_delay: Duration,
}

impl DtsClient {
    pub(crate) fn new(
        config: &DurableTaskReactionConfig,
        provider: Option<Arc<dyn IdentityProvider>>,
    ) -> Result<Self> {
        let timeout = Duration::from_millis(config.timeout_ms);
        let mut endpoint = Endpoint::from_shared(config.endpoint.clone())?
            .connect_timeout(timeout)
            .timeout(timeout);
        let provider = match config.authentication {
            Authentication::None => None,
            Authentication::Identity => {
                ensure!(
                    provider.is_some(),
                    "identity authentication requires an IdentityProvider"
                );
                endpoint = endpoint.tls_config(ClientTlsConfig::new().with_native_roots())?;
                provider
            }
        };
        Ok(Self {
            inner: TaskHubSidecarServiceClient::new(endpoint.connect_lazy()),
            hub: config.task_hub.parse()?,
            provider,
            scope: config.scope.clone(),
            timeout,
            max_retries: config.max_retries,
            retry_delay: Duration::from_millis(config.retry_delay_ms),
        })
    }

    pub(crate) async fn request<T>(&self, body: T) -> Result<Request<T>> {
        let mut request = Request::new(body);
        request.metadata_mut().insert("taskhub", self.hub.clone());
        request.metadata_mut().insert(
            "x-user-agent",
            MetadataValue::from_static(concat!(
                "drasi-reaction-durable-task/",
                env!("CARGO_PKG_VERSION")
            )),
        );
        if let Some(provider) = &self.provider {
            let context = CredentialContext::new().with_property("scope", &self.scope);
            let credentials =
                tokio::time::timeout(self.timeout, provider.get_credentials(&context))
                    .await
                    .context("DTS credential acquisition timed out")?
                    .context("DTS credential acquisition failed")?;
            let Credentials::Token { token, .. } = credentials else {
                bail!("DTS identity provider must return token credentials");
            };
            ensure!(
                !token.is_empty(),
                "DTS identity provider returned an empty token"
            );
            let mut authorization: MetadataValue<Ascii> = format!("Bearer {token}")
                .parse()
                .context("DTS identity provider returned invalid bearer metadata")?;
            authorization.set_sensitive(true);
            request
                .metadata_mut()
                .insert("authorization", authorization);
        }
        request.set_timeout(self.timeout);
        Ok(request)
    }

    pub(crate) async fn dispatch(&mut self, operation: &Dispatch) -> Result<()> {
        for attempt in 0..=self.max_retries {
            match self.send(operation).await {
                Ok(()) => return Ok(()),
                Err(error) if attempt < self.max_retries && transient(&error) => {
                    let delay = (self.retry_delay * (1 << attempt)).min(Duration::from_secs(30));
                    log::warn!(
                        "DTS delivery attempt {} failed: {error:#}; retrying in {delay:?}",
                        attempt + 1
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("each final attempt returns")
    }

    async fn get(&mut self, instance_id: &str) -> Result<Option<proto::OrchestrationState>> {
        let request = self
            .request(proto::GetInstanceRequest {
                instance_id: instance_id.to_owned(),
                get_inputs_and_outputs: true,
            })
            .await?;
        let response = match self.inner.get_instance(request).await {
            Ok(response) => response.into_inner(),
            Err(error) if error.code() == Code::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !response.exists {
            return Ok(None);
        }
        Ok(Some(
            response
                .orchestration_state
                .context("DTS returned exists=true without state")?,
        ))
    }

    async fn send(&mut self, operation: &Dispatch) -> Result<()> {
        match operation {
            Dispatch::Start {
                instance_id,
                orchestration,
                version,
                input,
                command_id,
                on_conflict,
            } => {
                let request = self
                    .request(proto::CreateInstanceRequest {
                        instance_id: instance_id.clone(),
                        name: orchestration.clone(),
                        version: Some(version.clone()),
                        input: Some(input.clone()),
                        orchestration_id_reuse_policy: Some(proto::OrchestrationIdReusePolicy {
                            replaceable_status: Vec::new(),
                        }),
                        tags: HashMap::from([(COMMAND_TAG.into(), command_id.clone())]),
                        ..Default::default()
                    })
                    .await?;
                match self.inner.start_instance(request).await {
                    Ok(response) => {
                        ensure!(
                            response.into_inner().instance_id == *instance_id,
                            "DTS returned an unexpected instance ID"
                        );
                        Ok(())
                    }
                    Err(error) if error.code() == Code::AlreadyExists => {
                        let existing = self.get(instance_id).await?;
                        if existing.as_ref().is_some_and(|state| {
                            state.name == *orchestration
                                && state.version.as_deref().unwrap_or_default() == version
                                && state.input.as_deref() == Some(input)
                                && state.tags.get(COMMAND_TAG) == Some(command_id)
                        }) {
                            return Ok(());
                        }
                        apply_policy(
                            *on_conflict,
                            instance_id,
                            "start conflicts with an existing instance",
                        )
                    }
                    Err(error) => Err(error.into()),
                }
            }
            Dispatch::Event {
                instance_id,
                event_name,
                input,
                on_missing,
                on_terminal,
            } => {
                let Some(state) = self.get(instance_id).await? else {
                    return apply_policy(*on_missing, instance_id, "event target does not exist");
                };
                if terminal(state.orchestration_status)? {
                    return apply_policy(*on_terminal, instance_id, "event target is terminal");
                }
                let request = self
                    .request(proto::RaiseEventRequest {
                        instance_id: instance_id.clone(),
                        name: event_name.clone(),
                        input: Some(input.clone()),
                    })
                    .await?;
                match self.inner.raise_event(request).await {
                    Ok(_) => Ok(()),
                    Err(error) if error.code() == Code::NotFound => {
                        apply_policy(*on_missing, instance_id, "event target disappeared")
                    }
                    Err(error) => Err(error.into()),
                }
            }
            Dispatch::Terminate {
                instance_id,
                output,
                recursive,
                on_missing,
            } => {
                let Some(state) = self.get(instance_id).await? else {
                    return apply_policy(
                        *on_missing,
                        instance_id,
                        "termination target does not exist",
                    );
                };
                if terminal(state.orchestration_status)? {
                    return Ok(());
                }
                let request = self
                    .request(proto::TerminateRequest {
                        instance_id: instance_id.clone(),
                        output: Some(output.clone()),
                        recursive: *recursive,
                    })
                    .await?;
                match self.inner.terminate_instance(request).await {
                    Ok(_) => Ok(()),
                    Err(error) if error.code() == Code::NotFound => {
                        apply_policy(*on_missing, instance_id, "termination target disappeared")
                    }
                    Err(error) => Err(error.into()),
                }
            }
        }
    }
}

fn apply_policy(policy: TargetPolicy, instance_id: &str, reason: &str) -> Result<()> {
    match policy {
        TargetPolicy::Error => bail!("DTS instance '{instance_id}': {reason}"),
        TargetPolicy::Ignore => {
            log::warn!("DTS instance '{instance_id}': {reason}; acknowledged per ignore policy");
            Ok(())
        }
    }
}

fn terminal(status: i32) -> Result<bool> {
    use proto::OrchestrationStatus;
    Ok(
        match OrchestrationStatus::try_from(status).context("unknown DTS orchestration status")? {
            OrchestrationStatus::Completed
            | OrchestrationStatus::Failed
            | OrchestrationStatus::Canceled
            | OrchestrationStatus::Terminated => true,
            OrchestrationStatus::Running
            | OrchestrationStatus::Pending
            | OrchestrationStatus::Suspended
            | OrchestrationStatus::ContinuedAsNew => false,
        },
    )
}

fn transient(error: &anyhow::Error) -> bool {
    error.downcast_ref::<tonic::Status>().is_some_and(|status| {
        matches!(
            status.code(),
            Code::Unavailable
                | Code::DeadlineExceeded
                | Code::ResourceExhausted
                | Code::Aborted
                | Code::Unknown
                | Code::Unauthenticated
        )
    })
}
