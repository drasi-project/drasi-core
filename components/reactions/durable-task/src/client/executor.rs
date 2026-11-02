// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{bail, ensure, Context, Result};
use drasi_lib::identity::IdentityProvider;
use tonic::Code;

use super::{DtsApi, GrpcDtsApi};
use crate::{
    config::{DurableTaskReactionConfig, TargetPolicy},
    dispatch::Dispatch,
    proto,
};

const COMMAND_TAG: &str = "drasi-command-id";
const START_TAG: &str = "drasi-start-fingerprint";

#[derive(Clone)]
pub(crate) struct DtsClient<A = GrpcDtsApi> {
    api: A,
    max_retries: u32,
    retry_delay_ms: u64,
}

impl DtsClient<GrpcDtsApi> {
    pub(crate) fn new(
        config: &DurableTaskReactionConfig,
        provider: Option<Arc<dyn IdentityProvider>>,
    ) -> Result<Self> {
        Ok(Self::with_api(config, GrpcDtsApi::new(config, provider)?))
    }
}

impl<A: DtsApi> DtsClient<A> {
    fn with_api(config: &DurableTaskReactionConfig, api: A) -> Self {
        Self {
            api,
            max_retries: config.max_retries,
            retry_delay_ms: config.retry_delay_ms,
        }
    }

    pub(crate) async fn dispatch(&mut self, operation: &Dispatch) -> Result<()> {
        for attempt in 0..=self.max_retries {
            match self.send(operation).await {
                Ok(()) => return Ok(()),
                Err(error) if attempt < self.max_retries && transient(&error) => {
                    let delay = retry_delay(self.retry_delay_ms, attempt);
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
                let fingerprint = uuid::Uuid::new_v5(
                    &uuid::Uuid::NAMESPACE_URL,
                    &serde_json::to_vec(&(orchestration, version, input))?,
                )
                .to_string();
                let request = proto::CreateInstanceRequest {
                    instance_id: instance_id.clone(),
                    name: orchestration.clone(),
                    version: Some(version.clone()),
                    input: Some(input.clone()),
                    orchestration_id_reuse_policy: Some(proto::OrchestrationIdReusePolicy {
                        replaceable_status: Vec::new(),
                    }),
                    tags: HashMap::from([
                        (COMMAND_TAG.into(), command_id.clone()),
                        (START_TAG.into(), fingerprint.clone()),
                    ]),
                    ..Default::default()
                };
                match self.api.start(request).await {
                    Ok(response) => {
                        ensure!(
                            response.instance_id == *instance_id,
                            "DTS returned an unexpected instance ID"
                        );
                        Ok(())
                    }
                    Err(error) if has_code(&error, Code::AlreadyExists) => {
                        let existing = self.api.get(instance_id).await?;
                        if existing.as_ref().is_some_and(|state| {
                            state.tags.get(COMMAND_TAG) == Some(command_id)
                                && state.tags.get(START_TAG) == Some(&fingerprint)
                        }) {
                            return Ok(());
                        }
                        apply_policy(
                            *on_conflict,
                            instance_id,
                            "start conflicts with an existing instance",
                        )
                    }
                    Err(error) => Err(error),
                }
            }
            Dispatch::Event {
                instance_id,
                event_name,
                input,
                on_missing,
                on_terminal,
            } => {
                let Some(state) = self.api.get(instance_id).await? else {
                    return apply_policy(*on_missing, instance_id, "event target does not exist");
                };
                if terminal(state.orchestration_status)? {
                    return apply_policy(*on_terminal, instance_id, "event target is terminal");
                }
                match self
                    .api
                    .raise_event(proto::RaiseEventRequest {
                        instance_id: instance_id.clone(),
                        name: event_name.clone(),
                        input: Some(input.clone()),
                    })
                    .await
                {
                    Ok(()) => Ok(()),
                    Err(error) if has_code(&error, Code::NotFound) => {
                        apply_policy(*on_missing, instance_id, "event target disappeared")
                    }
                    Err(error) => Err(error),
                }
            }
            Dispatch::Terminate {
                instance_id,
                output,
                recursive,
                on_missing,
            } => {
                let Some(state) = self.api.get(instance_id).await? else {
                    return apply_policy(
                        *on_missing,
                        instance_id,
                        "termination target does not exist",
                    );
                };
                if terminal(state.orchestration_status)? {
                    return Ok(());
                }
                match self
                    .api
                    .terminate(proto::TerminateRequest {
                        instance_id: instance_id.clone(),
                        output: Some(output.clone()),
                        recursive: *recursive,
                    })
                    .await
                {
                    Ok(()) => Ok(()),
                    Err(error) if has_code(&error, Code::NotFound) => {
                        apply_policy(*on_missing, instance_id, "termination target disappeared")
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }
}

fn retry_delay(base_ms: u64, attempt: u32) -> Duration {
    let cap = (base_ms * (1 << attempt)).min(30_000);
    Duration::from_millis(fastrand::u64(cap.div_ceil(2)..=cap))
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

fn has_code(error: &anyhow::Error, code: Code) -> bool {
    error
        .downcast_ref::<tonic::Status>()
        .is_some_and(|status| status.code() == code)
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

#[cfg(test)]
mod tests;
