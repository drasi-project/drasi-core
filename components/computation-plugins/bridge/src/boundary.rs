// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use drasi_lib::computation::v1::{
    ComponentRole, ProducerSession, ResourceCleanup, ResourceHandle, ResourceRole,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    receiver::Receiver, AcceptanceBoundary, BridgeConfig, BridgeEndpointControl, BridgeError,
    ErrorCode, Receipt, Result, TlsResource,
};

/// A sealed ingress frontier, fenced to this actual receiver lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressCut {
    pub receiver_epoch: Uuid,
    pub contract: [u8; 32],
    pub boundary: AcceptanceBoundary,
    pub session: Option<ProducerSession>,
    pub client_sequence: u64,
    pub position: u64,
    pub required_subscribers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressDrainStatus {
    pub cut: IngressCut,
    /// Volatile: every accepted envelope has been returned by source.next().
    /// Durable: every required journal subscriber has acknowledged this cut.
    pub drained: bool,
    pub queued: usize,
    pub source_dequeued_sequence: u64,
}

/// A normal native Component resource. It observes/controls the actual receiver,
/// never a second graph or an asynchronously projected component registry.
#[derive(Default)]
pub struct BridgeIngressControl {
    receiver: Mutex<Weak<Receiver>>,
}

impl BridgeIngressControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resource(self: &Arc<Self>) -> ResourceHandle {
        ResourceHandle::new(ResourceRole::Component, self.clone()).with_cleanup(self.clone())
    }

    pub(crate) fn bind(&self, receiver: &Arc<Receiver>) -> Result<()> {
        let mut slot = self
            .receiver
            .lock()
            .map_err(|_| BridgeError::invalid("ingress capability poisoned"))?;
        if slot.upgrade().is_some() {
            return Err(BridgeError::new(
                ErrorCode::Busy,
                "ingress capability already has an owner",
            ));
        }

        *slot = Arc::downgrade(receiver);
        Ok(())
    }

    pub(crate) fn unbind(&self, receiver: &Arc<Receiver>) -> Result<()> {
        let mut slot = self
            .receiver
            .lock()
            .map_err(|_| BridgeError::invalid("ingress capability poisoned"))?;
        if slot.ptr_eq(&Arc::downgrade(receiver)) {
            *slot = Weak::new();
        }
        Ok(())
    }

    fn receiver(&self) -> Result<Arc<Receiver>> {
        self.receiver
            .lock()
            .map_err(|_| BridgeError::invalid("ingress capability poisoned"))?
            .upgrade()
            .ok_or_else(|| {
                BridgeError::new(
                    ErrorCode::Closed,
                    "ingress capability is not bound to a running receiver",
                )
            })
    }

    /// Irreversible for this running receiver. A cancelled/timed-out call leaves
    /// admissions sealed; retry to resolve the same cut rather than reopen it.
    pub async fn seal(&self, timeout: Duration) -> Result<IngressCut> {
        crate::transport::deadline(timeout, self.receiver()?.seal()).await
    }

    pub async fn drain_status(&self, cut: &IngressCut) -> Result<IngressDrainStatus> {
        self.receiver()?.drain_status(cut).await
    }

    pub async fn sealed_cut(&self) -> Result<Option<IngressCut>> {
        self.receiver()?.sealed_cut().await
    }

    pub async fn wait_drained(
        &self,
        cut: &IngressCut,
        timeout: Duration,
    ) -> Result<IngressDrainStatus> {
        crate::transport::deadline(timeout, async {
            loop {
                let status = self.drain_status(cut).await?;
                if status.drained {
                    return Ok(status);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
    }
}

#[async_trait::async_trait]
impl ResourceCleanup for BridgeIngressControl {
    async fn shutdown(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.receiver
                .lock()
                .map_err(|_| BridgeError::invalid("ingress capability poisoned"))?
                .upgrade()
                .is_none(),
            "stop and join the ingress before releasing its control resource"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressStatus {
    pub in_flight: bool,
    /// Includes loaded persistent obligations and an unsuccessful current input,
    /// even when no input mapping could yet be committed.
    pub pending: bool,
    pub sealed: bool,
    pub last_receipt: Option<Receipt>,
}

#[derive(Default)]
pub struct BridgeEgressControl {
    state: Mutex<Option<EgressBinding>>,
}

struct EgressBinding {
    epoch: Uuid,
    status: EgressStatus,
    config: BridgeConfig,
    tls: Option<Arc<TlsResource>>,
}

impl BridgeEgressControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resource(self: &Arc<Self>) -> ResourceHandle {
        ResourceHandle::new(ResourceRole::Component, self.clone()).with_cleanup(self.clone())
    }

    pub fn snapshot(&self) -> Result<EgressStatus> {
        self.state
            .lock()
            .map_err(|_| BridgeError::invalid("egress capability poisoned"))?
            .as_ref()
            .map(|binding| binding.status.clone())
            .ok_or_else(|| {
                BridgeError::new(
                    ErrorCode::Closed,
                    "egress capability is not bound to a running sender",
                )
            })
    }

    pub(crate) fn bind(
        &self,
        config: BridgeConfig,
        tls: Option<Arc<TlsResource>>,
        pending: bool,
        receipt: Option<Receipt>,
    ) -> Result<Uuid> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| BridgeError::invalid("egress capability poisoned"))?;
        if state.is_some() {
            return Err(BridgeError::new(
                ErrorCode::Busy,
                "egress capability already has an owner",
            ));
        }
        let epoch = Uuid::new_v4();
        *state = Some(EgressBinding {
            epoch,
            config,
            tls,
            status: EgressStatus {
                in_flight: false,
                pending,
                sealed: false,
                last_receipt: receipt,
            },
        });
        Ok(epoch)
    }

    pub(crate) fn unbind(&self, epoch: Uuid) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| BridgeError::invalid("egress capability poisoned"))?;
        if state.as_ref().is_some_and(|binding| binding.epoch == epoch) {
            *state = None;
        }
        Ok(())
    }

    pub(crate) fn begin(self: &Arc<Self>, epoch: Uuid) -> Result<EgressOperation> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| BridgeError::invalid("egress capability poisoned"))?;
        let binding = state
            .as_mut()
            .ok_or_else(|| BridgeError::new(ErrorCode::Closed, "sender is stopped"))?;
        if binding.epoch != epoch {
            return Err(BridgeError::new(
                ErrorCode::StaleGeneration,
                "sender lifetime changed",
            ));
        }
        let status = &mut binding.status;
        if status.sealed {
            return Err(BridgeError::new(
                ErrorCode::Sealed,
                "sender is finishing; new inputs are forbidden",
            ));
        }
        status.in_flight = true;
        status.pending = true;
        Ok(EgressOperation {
            control: self.clone(),
            epoch,
        })
    }

    /// Call after normal graph quiescence of this sink and its entire upstream
    /// path. The returned frontier is admission/handoff evidence, not effects.
    pub async fn finish(&self, timeout: Duration) -> Result<IngressCut> {
        let (epoch, config, tls, receipt) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| BridgeError::invalid("egress capability poisoned"))?;
            let binding = state
                .as_mut()
                .ok_or_else(|| BridgeError::new(ErrorCode::Closed, "sender is stopped"))?;
            if binding.status.pending || binding.status.in_flight {
                return Err(BridgeError::new(
                    ErrorCode::AcceptanceUnknown,
                    "sender still owns an unresolved input",
                ));
            }
            binding.status.sealed = true;
            (
                binding.epoch,
                binding.config.clone(),
                binding.tls.clone(),
                binding.status.last_receipt.clone(),
            )
        };
        let cut = crate::transport::deadline(timeout, async {
            let mut endpoint =
                BridgeEndpointControl::connect(config, ComponentRole::Sink, tls).await?;
            endpoint.seal(timeout).await
        })
        .await?;
        let matches = match receipt {
            Some(receipt) => {
                cut.session.as_ref() == Some(&receipt.admission.session)
                    && cut.client_sequence == receipt.admission.sequence
                    && cut.position == receipt.admission.position
                    && cut.contract == receipt.contract
                    && cut.boundary == receipt.boundary
            }
            None => cut.client_sequence == 0 && cut.position == 0,
        };
        if !matches {
            return Err(BridgeError::new(
                ErrorCode::StateLost,
                "receiver cut differs from the sender's resolved receipt",
            ));
        }
        if self
            .state
            .lock()
            .map_err(|_| BridgeError::invalid("egress capability poisoned"))?
            .as_ref()
            .is_none_or(|binding| binding.epoch != epoch)
        {
            return Err(BridgeError::new(
                ErrorCode::StaleGeneration,
                "sender lifetime changed while finishing",
            ));
        }
        Ok(cut)
    }
}

#[async_trait::async_trait]
impl ResourceCleanup for BridgeEgressControl {
    async fn shutdown(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.state
                .lock()
                .map_err(|_| BridgeError::invalid("egress capability poisoned"))?
                .is_none(),
            "stop the sender before releasing its control resource"
        );
        Ok(())
    }
}

pub(crate) struct EgressOperation {
    control: Arc<BridgeEgressControl>,
    epoch: Uuid,
}

impl EgressOperation {
    pub(crate) fn completed(&self, receipt: Option<Receipt>) -> Result<()> {
        let mut state = self
            .control
            .state
            .lock()
            .map_err(|_| BridgeError::invalid("egress capability poisoned"))?;
        let binding = state
            .as_mut()
            .ok_or_else(|| BridgeError::new(ErrorCode::Closed, "sender is stopped"))?;
        if binding.epoch != self.epoch {
            return Err(BridgeError::new(
                ErrorCode::StaleGeneration,
                "sender lifetime changed",
            ));
        }
        let status = &mut binding.status;
        status.pending = false;
        status.last_receipt = receipt;
        Ok(())
    }
}

impl Drop for EgressOperation {
    fn drop(&mut self) {
        if let Ok(mut state) = self.control.state.lock() {
            if let Some(binding) = state.as_mut() {
                if binding.epoch == self.epoch {
                    binding.status.in_flight = false;
                }
            }
        }
    }
}
