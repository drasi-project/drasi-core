// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{future::Future, pin::Pin, sync::Arc};

use drasi_computation_contracts::{BootstrapCompletion, BootstrapState, BootstrapStatus};
use drasi_computation_plugin_abi::operation;
use drasi_computation_plugin_sdk::{bootstrap::Completed, wire, Scope};
use drasi_lib::computation::v1::{
    BinaryEnvelopeCodec, BootstrapPreparation, ComponentId, ComputationBootstrapProvider,
    InputBinding, InputEnvelope, PortId,
};
use futures_util::StreamExt;

use super::proxy::RemoteComponent;

/// An existing provider, not an output snapshot or a fabricated source cursor.
pub struct NativeBootstrapResource(pub Arc<dyn ComputationBootstrapProvider>);

type CleanupOperation = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;

pub(super) struct BootstrapBinding {
    resource: Arc<NativeBootstrapResource>,
    input: PortId,
    cleanup_required: bool,
    stopping: tokio::sync::Mutex<Option<CleanupOperation>>,
    state: Option<Arc<dyn BootstrapState>>,
}

impl BootstrapBinding {
    pub(super) fn new(
        resource: Arc<NativeBootstrapResource>,
        input: PortId,
        scope: &Scope,
        component: &ComponentId,
        processing: Option<&super::processing::ProcessingBinding>,
    ) -> anyhow::Result<Self> {
        resource
            .0
            .validate_query_scope(&scope.instance_id, &scope.graph_id, component)?;
        let state = match processing {
            Some(processing) => {
                let owner = processing.progress_owner().ok_or_else(|| {
                    anyhow::anyhow!("durable bootstrap has no committed processing owner")
                })?;
                if let Some(reader) = resource.0.recovery_reader() {
                    anyhow::ensure!(
                        reader
                            .local_owner()
                            .is_some_and(|reader| Arc::ptr_eq(&reader, &owner)),
                        "bootstrap provider is not bound to this actual processing owner"
                    );
                }
                Some(processing.bootstrap_state())
            }
            None => {
                anyhow::ensure!(
                    resource.0.recovery_reader().is_none(),
                    "quiescent bootstrap does not support progress-bound providers"
                );
                None
            }
        };
        Ok(Self {
            resource,
            input,
            cleanup_required: false,
            stopping: tokio::sync::Mutex::new(None),
            state,
        })
    }

    pub(super) fn needs_cleanup(&self) -> bool {
        self.cleanup_required
    }

    pub(super) async fn initialize(
        &mut self,
        component: &RemoteComponent,
        codec: &BinaryEnvelopeCodec,
        bindings: Option<&[InputBinding]>,
    ) -> anyhow::Result<Vec<InputEnvelope>> {
        anyhow::ensure!(
            !self.cleanup_required,
            "bootstrap requires cleanup before reuse"
        );
        self.cleanup_required = true;
        let provider = self.resource.0.clone();
        if let Some(bindings) = bindings {
            component
                .unit(operation::BIND_INPUTS, &wire::encode(&bindings)?)
                .await?;
        }
        let mut snapshot = if let Some(state) = self.state.clone() {
            let mut status: BootstrapStatus =
                wire::decode(&component.call(operation::BOOTSTRAP_STATUS, &[]).await?)?;
            anyhow::ensure!(
                status.configured,
                "bootstrap provider bound to a transformer without configured initialization"
            );
            let preparation =
                match provider.prepare_with_state(state.as_ref()).await {
                    Err(error)
                        if status.reset_allowed
                            && matches!(
                    error.downcast_ref::<drasi_computation_contracts::OutputReadError>(),
                    Some(drasi_computation_contracts::OutputReadError::Gap { .. }
                        | drasi_computation_contracts::OutputReadError::EpochChanged { .. })) =>
                    {
                        BootstrapPreparation::ResetRequired
                    }
                    result => result?,
                };
            if preparation != BootstrapPreparation::Ready
                || status.complete && provider.has_pending_snapshot()?
            {
                if !status.reset_allowed {
                    return Err(anyhow::Error::new(
                        drasi_computation_contracts::QueryRecoveryError::SourceResetRequired,
                    )
                    .context(
                        "bootstrap provider requires an explicit reset under Strict recovery",
                    ));
                }
                self.stop().await?;
                component.unit(operation::BOOTSTRAP_RESET, &[]).await?;
                self.cleanup_required = true;
                anyhow::ensure!(
                    provider.prepare_with_state(state.as_ref()).await?
                        == BootstrapPreparation::Ready,
                    "bootstrap provider still requires reset after AutoReset"
                );
                status = wire::decode(&component.call(operation::BOOTSTRAP_STATUS, &[]).await?)?;
                anyhow::ensure!(
                    status.configured && !status.complete,
                    "transformer did not reset its initialization state"
                );
            }
            if status.complete {
                let replay = self.input_replay(provider.as_ref())?;
                self.stop().await?;
                component
                    .unit(
                        operation::BOOTSTRAP_COMPLETE,
                        &wire::encode(&Completed::from(BootstrapCompletion::default()))?,
                    )
                    .await?;
                return Ok(replay);
            }
            provider.snapshot_with_state(state.as_ref()).await?
        } else {
            anyhow::ensure!(
                provider.prepare().await? == BootstrapPreparation::Ready,
                "quiescent bootstrap requires ready preparation"
            );
            let snapshot = provider.snapshot().await?;
            anyhow::ensure!(
                snapshot.watermarks.is_empty(),
                "quiescent bootstrap does not support source watermarks"
            );
            snapshot
        };
        for watermark in &snapshot.watermarks {
            watermark.validate()?;
        }
        let mut active_bindings = bindings.map(<[InputBinding]>::to_vec);
        while let Some(envelope) = snapshot.changes.next().await {
            let input = InputEnvelope {
                port: self.input.clone(),
                envelope: envelope?,
            };
            if let Some(bindings) = bindings {
                let mut selected = bindings.to_vec();
                if let Some(origin) = provider.input_origin(input.envelope.system().stream())? {
                    selected.retain(|binding| {
                        binding.input_port != input.port
                            || &binding.stream != input.envelope.system().stream()
                    });
                    selected.push(InputBinding {
                        input_port: input.port.clone(),
                        output: origin,
                        stream: input.envelope.system().stream().clone(),
                    });
                }
                if active_bindings.as_ref() != Some(&selected) {
                    component
                        .unit(operation::BIND_INPUTS, &wire::encode(&selected)?)
                        .await?;
                    active_bindings = Some(selected);
                }
            }
            let bytes = wire::encode(&wire::Envelope::input(&input, codec)?)?;
            component.unit(operation::BOOTSTRAP_INPUT, &bytes).await?;
        }
        let mut watermarks = std::mem::take(&mut snapshot.watermarks);
        drop(snapshot);
        watermarks.extend(provider.complete_snapshot().await?);
        let state = provider.completion_state()?;
        let replay = self.input_replay(provider.as_ref())?;
        let completion = if self.state.is_some() {
            wire::encode(&Completed::from(BootstrapCompletion { watermarks, state }))?
        } else {
            anyhow::ensure!(
                watermarks.is_empty() && state.is_none(),
                "quiescent bootstrap does not support durable completion state or watermarks"
            );
            Vec::new()
        };
        self.stop().await?;
        if let Some(bindings) = bindings {
            component
                .unit(operation::BIND_INPUTS, &wire::encode(&bindings)?)
                .await?;
        }
        component
            .unit(operation::BOOTSTRAP_COMPLETE, &completion)
            .await?;
        Ok(replay)
    }

    fn input_replay(
        &self,
        provider: &dyn ComputationBootstrapProvider,
    ) -> anyhow::Result<Vec<InputEnvelope>> {
        let replay = provider.take_input_replay()?;
        anyhow::ensure!(
            self.state.is_some() || replay.is_empty(),
            "quiescent bootstrap does not support prepared input replay"
        );
        Ok(replay
            .into_iter()
            .map(|envelope| InputEnvelope {
                port: self.input.clone(),
                envelope,
            })
            .collect())
    }

    pub(super) async fn stop(&mut self) -> anyhow::Result<()> {
        if !self.cleanup_required {
            return Ok(());
        }
        let stopping = self.stopping.get_mut();
        if stopping.is_none() {
            let resource = self.resource.clone();
            *stopping = Some(Box::pin(async move { resource.0.stop().await }));
        }
        let result = stopping.as_mut().expect("owned bootstrap cleanup").await;
        *stopping = None;
        result?;
        self.cleanup_required = false;
        Ok(())
    }
}

impl Drop for BootstrapBinding {
    fn drop(&mut self) {
        if self.cleanup_required {
            log::error!("native bootstrap binding dropped before awaited provider cleanup");
        }
    }
}
