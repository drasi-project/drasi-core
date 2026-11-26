// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use drasi_computation_bridge_common::{
    client::{BridgeControl, Client},
    protocol::{Receipt, Welcome},
    state::{LogicalInput, MappingFile, SenderState},
    BridgeConfig, BridgeError, ErrorCode, PortableEnvelopeCodec, TlsResource,
};
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentRole, DeliveryFailure, DeliveryScope, InputEnvelope,
    SinkCompletion,
};
use drasi_computation_plugin_sdk::{
    lifecycle::{
        DisposeContext, InitializeContext, LifecycleError, LifecycleResult, PauseContext,
        ResumeContext, StartContext, StopContext,
    },
    NativeComponent, NativeSink,
};

/// The sending endpoint. Construction opens nothing; Start loads (and owns)
/// the sender mapping and Stop releases it.
pub struct BridgeSink {
    config: BridgeConfig,
    graph_id: String,
    descriptor: ComponentDescriptor,
    scope: DeliveryScope,
    codec: PortableEnvelopeCodec,
    state: Option<SenderState>,
    client: Option<Client>,
}

impl BridgeSink {
    pub fn new(config: BridgeConfig, graph_id: String) -> anyhow::Result<Self> {
        config.validate_role(ComponentRole::Sink)?;
        let descriptor = config.descriptor(ComponentRole::Sink)?;
        drasi_computation_bridge_common::config::identifier(&graph_id)?;
        let scope = DeliveryScope::new(
            config.sender_instance.clone(),
            graph_id.clone(),
            descriptor.id().clone(),
        )?;
        let codec = PortableEnvelopeCodec::new(
            config.delivery,
            config.namespace.clone(),
            config.limits.clone(),
        )?;
        Ok(Self {
            config,
            graph_id,
            descriptor,
            scope,
            codec,
            state: None,
            client: None,
        })
    }

    /// The link's read-only control lane while started.
    pub fn control(&self) -> Option<BridgeControl> {
        self.client.as_ref().map(Client::control)
    }

    /// Local evidence only: a receipt proves receiver admission, never graph drain.
    pub fn last_receipt(&self) -> Option<&Receipt> {
        self.state
            .as_ref()
            .and_then(SenderState::last)
            .and_then(|mapping| mapping.receipt.as_ref())
    }

    fn mapping(&self) -> anyhow::Result<Option<MappingFile>> {
        self.config
            .mapping_directory
            .as_ref()
            .map(|directory| {
                MappingFile::new(
                    Path::new(directory),
                    &self.config.sender_instance,
                    &self.graph_id,
                    self.config.sender_endpoint.as_str(),
                )
            })
            .transpose()
    }

    fn tls(&self) -> anyhow::Result<Option<Arc<TlsResource>>> {
        let Some(recipe) = &self.config.tls else {
            return Ok(None);
        };
        let tls = recipe.resolve(Path::new("."))?;
        tls.validate_role(false)?;
        Ok(Some(tls))
    }

    fn check_receiver_state(&self, welcome: &Welcome) -> anyhow::Result<()> {
        let last = self.state.as_ref().and_then(SenderState::last);
        let expected = last.map_or(Some(1), |mapping| {
            if mapping.receipt.is_some() {
                mapping.client_sequence.checked_add(1)
            } else {
                Some(mapping.client_sequence)
            }
        });
        let pending_next = last
            .filter(|mapping| mapping.receipt.is_none())
            .and_then(|mapping| mapping.client_sequence.checked_add(1));
        anyhow::ensure!(
            welcome.next_sequence == expected
                || (pending_next.is_some() && welcome.next_sequence == pending_next),
            BridgeError::new(
                ErrorCode::StateLost,
                "receiver progress differs from the saved sender mapping"
            )
        );
        Ok(())
    }

    async fn release(&mut self) -> anyhow::Result<()> {
        if let Some(client) = &mut self.client {
            client.invalidate().await;
        }
        self.client = None;
        if let Some(mut state) = self.state.take() {
            state.shutdown().await?;
        }
        Ok(())
    }
}

#[async_trait]
impl NativeComponent for BridgeSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }

    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        if let Some(recipe) = &self.config.tls {
            recipe.validate()?;
        }
        Ok(())
    }

    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        if self.state.is_some() || self.client.is_some() {
            return Err(LifecycleError::terminal(anyhow::anyhow!(
                "bridge sink requires completed cleanup before restart"
            )));
        }
        let tls = self.tls()?;
        let mut state = SenderState::new(self.config.contract(false), self.mapping()?)?;
        let loaded = tokio::select! {
            biased;
            _ = ctx.cancellation().cancelled() => Err(LifecycleError::cancelled()),
            loaded = state.load() => loaded.map_err(LifecycleError::from),
        };
        if let Err(error) = loaded {
            state.shutdown().await?;
            return Err(error);
        }
        self.state = Some(state);
        self.client = Some(Client::new(self.config.clone(), tls));
        Ok(())
    }

    /// The Host calls no `handle` while paused and lets the one in flight
    /// settle first; the sink has no worker of its own.
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        Ok(())
    }

    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        Ok(())
    }

    async fn stop(&mut self, _ctx: &StopContext) -> LifecycleResult {
        self.release().await?;
        Ok(())
    }

    /// Erase removes the durable sender mapping, idempotently; a volatile
    /// sender owns nothing.
    async fn dispose(&mut self, ctx: &DisposeContext) -> LifecycleResult {
        self.release().await?;
        if ctx.erases_owned_state() {
            if let Some(mapping) = self.mapping()? {
                tokio::task::spawn_blocking(move || mapping.erase())
                    .await
                    .map_err(anyhow::Error::from)??;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl NativeSink for BridgeSink {
    fn completion(&self) -> SinkCompletion {
        self.config.completion()
    }

    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.state.is_some() && self.client.is_some(),
            "bridge sink is not started"
        );
        anyhow::ensure!(
            input.port.as_str() == "in" && input.envelope.system().stream() == &self.config.stream,
            "bridge sink accepts only its configured input stream and port"
        );
        let bytes = self.codec.encode(&input.envelope)?;
        let identity = LogicalInput::identify(
            self.config.profile,
            &self.scope,
            &input,
            &self.codec,
            &bytes,
        )?;
        let mut last_error = BridgeError::new(ErrorCode::Closed, "bridge has not connected");
        for attempt in 0..self.config.limits.retry_attempts {
            let (Some(state), Some(client)) = (self.state.as_ref(), self.client.as_mut()) else {
                anyhow::bail!("bridge sink is not started");
            };
            let expected = state.session().cloned();
            let welcome = client.connect(expected.as_ref()).await;
            let result: drasi_computation_bridge_common::Result<()> = match welcome {
                Ok(welcome) => {
                    self.check_receiver_state(&welcome)?;
                    let (Some(state), Some(client)) = (self.state.as_mut(), self.client.as_mut())
                    else {
                        anyhow::bail!("bridge sink is not started");
                    };
                    state.bind_session(welcome.session.clone()).await?;
                    // This commit precedes every first data transmission.
                    state.prepare(identity.clone(), bytes.clone()).await?;
                    let mapping = state
                        .last()
                        .ok_or_else(|| anyhow::anyhow!("prepared sender mapping is missing"))?;
                    if mapping.receipt.is_some() {
                        return Ok(());
                    }
                    let sequence = mapping.client_sequence;
                    let receipt = match client.control().receipt(&welcome.session, sequence).await {
                        Ok(Some(receipt)) => Ok(receipt),
                        Ok(None) => {
                            client
                                .send(&welcome.session, sequence, &mapping.envelope)
                                .await
                        }
                        Err(error) => Err(error),
                    };
                    match receipt {
                        Ok(receipt) => {
                            state.complete(receipt).await?;
                            return Ok(());
                        }
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                let retryable = error.retryable();
                last_error = error;
                // Unknown/partial I/O can only reconnect with the saved mapping.
                // Even Busy reopens, fencing any delayed old-connection bytes.
                if let Some(client) = self.client.as_mut() {
                    client.invalidate().await;
                }
                if !retryable {
                    return Err(delivery_failure(last_error));
                }
            }
            if attempt + 1 < self.config.limits.retry_attempts {
                tokio::time::sleep(std::time::Duration::from_millis(
                    self.config.limits.retry_delay_ms,
                ))
                .await;
            }
        }
        Err(delivery_failure(last_error))
    }
}

/// Classifies a bridge failure for the graph's delivery-failure model: link
/// and connection failures are transient; a receiver that rejects this
/// input's content can never accept it; lost, conflicting or expired link
/// state ends the activation, because no retry of this input can repair it.
fn delivery_failure(error: BridgeError) -> anyhow::Error {
    if error.retryable() {
        DeliveryFailure::transient(error).into()
    } else if matches!(error.code, ErrorCode::Invalid | ErrorCode::Incompatible) {
        DeliveryFailure::permanent(error).into()
    } else {
        LifecycleError::terminal(error).into()
    }
}
