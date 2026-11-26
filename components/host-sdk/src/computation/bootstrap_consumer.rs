// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{future::Future, pin::Pin, sync::Arc};

use drasi_computation_contracts::{
    BootstrapAcceptance, BootstrapCompletion, BootstrapState, BootstrapStatus,
};
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
    mode: Mode,
}

enum Mode {
    Quiescent,
    Bounded,
    Durable(Arc<dyn BootstrapState>),
}

/// The graph's state-aware session API needs to know this is a fresh read.
/// There is deliberately no checkpoint to restore, update, or mistake for
/// durable provider intent.
struct FreshRead;

#[async_trait::async_trait]
impl BootstrapState for FreshRead {
    fn durability(&self) -> drasi_core_types::interface::StorageDurability {
        drasi_core_types::interface::StorageDurability::VOLATILE
    }
    async fn read(&self) -> anyhow::Result<Option<bytes::Bytes>> {
        Ok(None)
    }
    async fn write(&self, _: bytes::Bytes) -> anyhow::Result<()> {
        anyhow::bail!("volatile bootstrap cannot store provider handover state")
    }
}

impl BootstrapBinding {
    pub(super) fn new(
        resource: Arc<NativeBootstrapResource>,
        input: PortId,
        profile: BootstrapAcceptance,
        scope: &Scope,
        component: &ComponentId,
        processing: Option<&super::processing::ProcessingBinding>,
    ) -> anyhow::Result<Self> {
        resource
            .0
            .validate_query_scope(&scope.instance_id, &scope.graph_id, component)?;
        let mode = match profile {
            BootstrapAcceptance::Durable => {
                let processing = processing.ok_or_else(|| {
                    anyhow::anyhow!("durable bootstrap requires indexed processing")
                })?;
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
                Mode::Durable(processing.bootstrap_state())
            }
            BootstrapAcceptance::Quiescent | BootstrapAcceptance::Bounded => {
                anyhow::ensure!(
                    processing.is_none() && resource.0.recovery_reader().is_none(),
                    "volatile bootstrap does not support progress-bound providers"
                );
                if profile == BootstrapAcceptance::Bounded {
                    Mode::Bounded
                } else {
                    Mode::Quiescent
                }
            }
        };
        Ok(Self {
            resource,
            input,
            cleanup_required: false,
            stopping: tokio::sync::Mutex::new(None),
            mode,
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
        rebuild_destinations: Option<(
            &mut super::processing::ProcessingBinding,
            &drasi_lib::computation::v1::OutputBindings,
        )>,
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
        let mut snapshot = if let Mode::Durable(state) = &self.mode {
            let state = state.clone();
            let mut status: BootstrapStatus =
                wire::decode(&component.call(operation::BOOTSTRAP_STATUS, &[]).await?)?;
            anyhow::ensure!(
                status.configured,
                "bootstrap provider bound to a transformer without configured initialization"
            );
            let mut rebuild = false;
            let preparation = match provider.prepare_with_state(state.as_ref()).await {
                Err(error)
                    if status.reset_allowed
                        && matches!(
                            error.downcast_ref::<drasi_computation_contracts::OutputReadError>(),
                            Some(
                                drasi_computation_contracts::OutputReadError::Gap { .. }
                                    | drasi_computation_contracts::OutputReadError::EpochChanged { .. }
                            )
                        ) =>
                {
                    if let (
                        Some([binding]),
                        Some(drasi_computation_contracts::OutputReadError::Gap {
                            requested,
                            latest,
                            ..
                        }),
                    ) = (bindings, error.downcast_ref())
                    {
                        if status.complete && requested < latest {
                            self.stop().await?;
                            if let Some(after) = provider
                                .prepare_gap_rebuild(state.as_ref(), binding, *requested)
                                .await?
                            {
                                self.cleanup_required = true;
                                rebuild = wire::decode(
                                    &component
                                        .call(
                                            operation::BOOTSTRAP_REBUILD,
                                            &wire::encode(&Some(after))?,
                                        )
                                        .await?,
                                )?;
                            }
                        }
                    }
                    BootstrapPreparation::ResetRequired
                }
                result => result?,
            };
            // A completed view that never read an input cannot resume one. When
            // a dependent installed that view, its first input arrives as a change.
            if !rebuild && status.complete && preparation == BootstrapPreparation::ResetRequired {
                if let Some([binding]) = bindings {
                    self.stop().await?;
                    if provider
                        .prepare_first_input_rebuild(state.as_ref(), binding)
                        .await?
                    {
                        self.cleanup_required = true;
                        rebuild = wire::decode(
                            &component
                                .call(
                                    operation::BOOTSTRAP_REBUILD,
                                    &wire::encode(
                                        &None::<drasi_computation_contracts::OutputCursor>,
                                    )?,
                                )
                                .await?,
                        )?;
                    }
                }
            }
            if rebuild {
                let (owner, destinations) = rebuild_destinations.ok_or_else(|| {
                    anyhow::anyhow!("query rebuild requires verified output destinations")
                })?;
                owner.bind_outputs(destinations).await?;
            } else if preparation != BootstrapPreparation::Ready
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
            if status.complete && !rebuild {
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
            provider.open_snapshot(state).await?
        } else {
            let mut preparation = provider.prepare_with_state(&FreshRead).await?;
            if preparation == BootstrapPreparation::RefreshVolatile {
                self.stop().await?;
                self.cleanup_required = true;
                preparation = provider.prepare_with_state(&FreshRead).await?;
            }
            anyhow::ensure!(
                preparation == BootstrapPreparation::Ready,
                "volatile bootstrap requires a fresh snapshot"
            );
            let snapshot = provider.open_snapshot(Arc::new(FreshRead)).await?;
            anyhow::ensure!(
                !matches!(self.mode, Mode::Quiescent) || snapshot.watermarks.is_empty(),
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
                execution_context: drasi_lib::observability::ExecutionContext::current(),
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
        let completion = if !matches!(self.mode, Mode::Quiescent) {
            if matches!(self.mode, Mode::Bounded) {
                anyhow::ensure!(
                    state.is_none() && watermarks.iter().all(|w| w.producer.is_some()),
                    "bounded bootstrap accepts only quiescent completion or output cuts"
                );
            }
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
            matches!(self.mode, Mode::Durable(_)) || replay.is_empty(),
            "volatile bootstrap does not support prepared input replay"
        );
        Ok(replay
            .into_iter()
            .map(|envelope| InputEnvelope {
                port: self.input.clone(),
                envelope,
                execution_context: drasi_lib::observability::ExecutionContext::current(),
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
