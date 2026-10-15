// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;

use async_trait::async_trait;
use drasi_core::computation::{ComputationIndexProvider, ComputationResourceCleanup};
use drasi_lib::computation::v1::{
    ComponentDescriptor, ComponentRole, ComputationComponent, DeliveryScope, EnvelopeSink,
    InputEnvelope, SinkCompletion,
};
use sha2::{Digest, Sha256};

use crate::{
    client::{BridgeControl, Client},
    protocol::{Receipt, Welcome},
    state::{LogicalInput, SenderState},
    BridgeConfig, BridgeEgressControl, BridgeError, ErrorCode, PortableEnvelopeCodec, Profile,
    TlsResource,
};

pub struct BridgeSink {
    config: BridgeConfig,
    graph_id: String,
    descriptor: ComponentDescriptor,
    scope: DeliveryScope,
    provider: Option<Arc<dyn ComputationIndexProvider>>,
    tls: Option<Arc<TlsResource>>,
    codec: PortableEnvelopeCodec,
    state: Option<SenderState>,
    client: Client,
    failed_cleanup: Option<Arc<dyn ComputationResourceCleanup>>,
    started: bool,
    egress_control: Arc<BridgeEgressControl>,
    control_epoch: Option<uuid::Uuid>,
}

impl BridgeSink {
    pub fn new(
        config: BridgeConfig,
        graph_id: String,
        provider: Option<Arc<dyn ComputationIndexProvider>>,
        tls: Option<Arc<TlsResource>>,
    ) -> anyhow::Result<Self> {
        let descriptor = config.descriptor(ComponentRole::Sink)?;
        crate::config::identifier(&graph_id)?;
        anyhow::ensure!(
            (config.profile == Profile::Durable) == provider.is_some(),
            "durable sink requires mapping storage; volatile sink forbids it"
        );
        anyhow::ensure!(
            config.transport.is_tls() == tls.is_some(),
            "TLS transport requires exactly its identity resource"
        );
        if let Some(tls) = &tls {
            tls.validate_role(false)?;
        }
        let scope = DeliveryScope::new(
            config.sender_instance.clone(),
            graph_id.clone(),
            descriptor.id().clone(),
        )?;
        let codec = PortableEnvelopeCodec::new(
            config.schema,
            config.namespace.clone(),
            config.limits.clone(),
        )?;
        let client = Client::new(config.clone(), tls.clone());
        Ok(Self {
            config,
            graph_id,
            descriptor,
            scope,
            provider,
            tls,
            codec,
            state: None,
            client,
            failed_cleanup: None,
            started: false,
            egress_control: Arc::new(BridgeEgressControl::new()),
            control_epoch: None,
        })
    }

    pub fn control(&self) -> BridgeControl {
        self.client.control()
    }

    pub fn egress_control(&self) -> Arc<BridgeEgressControl> {
        self.egress_control.clone()
    }

    pub fn with_egress_control(mut self, control: Arc<BridgeEgressControl>) -> Self {
        self.egress_control = control;
        self
    }

    /// Local evidence only: a receipt proves receiver admission, never graph drain.
    pub fn last_receipt(&self) -> Option<&Receipt> {
        self.state
            .as_ref()
            .and_then(SenderState::last)
            .and_then(|mapping| mapping.receipt.as_ref())
    }

    fn check_receiver_state(&self, welcome: &Welcome) -> anyhow::Result<()> {
        let expected = self
            .state
            .as_ref()
            .and_then(SenderState::last)
            .map_or(Some(1), |mapping| {
                if mapping.receipt.is_some() {
                    mapping.client_sequence.checked_add(1)
                } else {
                    Some(mapping.client_sequence)
                }
            });
        let pending_next = self
            .state
            .as_ref()
            .and_then(SenderState::last)
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
}

#[async_trait]
impl ComputationComponent for BridgeSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.started && self.state.is_none() && self.failed_cleanup.is_none(),
            "bridge sink requires completed cleanup before restart"
        );
        let indexes = if let Some(provider) = &self.provider {
            let namespace = serde_json::to_vec(&(
                &self.config.sender_instance,
                &self.graph_id,
                &self.config.sender_endpoint,
            ))?;
            let graph = format!("drasi.bridge/{:x}", Sha256::digest(namespace));
            let indexes = provider
                .create_indexes(&graph, &self.config.sender_endpoint)
                .await?;
            self.failed_cleanup = indexes.cleanup().cloned();
            indexes.durability().require(self.config.failure_scope)?;
            indexes.atomic_result_transaction()?;
            anyhow::ensure!(
                indexes.cleanup().is_some(),
                "sender mapping provider requires asynchronous cleanup"
            );
            Some(indexes)
        } else {
            None
        };
        self.state = Some(SenderState::new(self.config.contract(false), indexes)?);
        self.failed_cleanup = None;
        self.state.as_mut().expect("owned mapping").load().await?;
        self.client.invalidate().await;
        let mapping = self.state.as_ref().and_then(SenderState::last);
        self.control_epoch = Some(self.egress_control.bind(
            self.config.clone(),
            self.tls.clone(),
            mapping.is_some_and(|mapping| mapping.receipt.is_none()),
            mapping.and_then(|mapping| mapping.receipt.clone()),
        )?);
        self.started = true;
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.started = false;
        self.client.invalidate().await;
        if let Some(state) = &self.state {
            state.shutdown().await?;
        }
        if let Some(cleanup) = &self.failed_cleanup {
            cleanup.cancel();
            cleanup.shutdown().await?;
        }
        self.state = None;
        self.failed_cleanup = None;
        if let Some(epoch) = self.control_epoch {
            self.egress_control.unbind(epoch)?;
            self.control_epoch = None;
        }
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for BridgeSink {
    fn completion(&self) -> SinkCompletion {
        self.config.completion()
    }

    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        anyhow::ensure!(self.started, "bridge sink is not started");
        let operation = self
            .egress_control
            .begin(self.control_epoch.expect("started control"))?;
        anyhow::ensure!(
            input.port.as_str() == "in"
                && input.envelope.system().stream().as_str() == self.config.stream,
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
            let expected = self
                .state
                .as_ref()
                .expect("started mapping")
                .session()
                .cloned();
            let welcome = self.client.connect(expected.as_ref()).await;
            let result: drasi_computation_bridge_common::Result<()> = match welcome {
                Ok(welcome) => {
                    self.check_receiver_state(&welcome)?;
                    let state = self.state.as_mut().expect("started mapping");
                    state.bind_session(welcome.session.clone()).await?;
                    // This commit precedes every first data transmission.
                    state.prepare(identity.clone(), bytes.clone()).await?;
                    let mapping = state.last().expect("prepared mapping");
                    if mapping.receipt.is_some() {
                        operation.completed(mapping.receipt.clone())?;
                        return Ok(());
                    }
                    let sequence = mapping.client_sequence;
                    let receipt = match self
                        .client
                        .control()
                        .receipt(&welcome.session, sequence)
                        .await
                    {
                        Ok(Some(receipt)) => Ok(receipt),
                        Ok(None) => {
                            self.client
                                .send(&welcome.session, sequence, &mapping.envelope)
                                .await
                        }
                        Err(error) => Err(error),
                    };
                    match receipt {
                        Ok(receipt) => {
                            state.complete(receipt).await?;
                            operation.completed(
                                state.last().and_then(|mapping| mapping.receipt.clone()),
                            )?;
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
                self.client.invalidate().await;
                if !retryable {
                    return Err(last_error.into());
                }
            }
            if attempt + 1 < self.config.limits.retry_attempts {
                tokio::time::sleep(std::time::Duration::from_millis(
                    self.config.limits.retry_delay_ms,
                ))
                .await;
            }
        }
        Err(last_error.into())
    }
}
