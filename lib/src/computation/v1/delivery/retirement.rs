// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::{Arc, Mutex, Weak};

use drasi_core::computation::ComputationIndexProvider;

use super::{DeliveryOptions, DeliveryRunner};

/// A delivery binding; ordinary sinks do not acquire this service.
pub struct ConsumerDeliveryResource {
    pub provider: Arc<dyn ComputationIndexProvider>,
    pub options: DeliveryOptions,
}

#[derive(Default)]
struct State {
    drained: Option<bool>,
    closed: bool,
    held: bool,
    retired: bool,
}

/// Positive ledger evidence from an initialized, successfully closed owner.
/// The graph must also freeze its stopped generation, excluding reconstruction.
/// This view retains neither storage nor a replacement delivery runner.
pub struct DeliveryRetirementState {
    provider: Weak<dyn ComputationIndexProvider>,
    state: Mutex<State>,
}

impl std::fmt::Debug for DeliveryRetirementState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DeliveryRetirementState")
    }
}

impl DeliveryRetirementState {
    pub fn new(provider: &Arc<dyn ComputationIndexProvider>) -> Arc<Self> {
        Arc::new(Self {
            provider: Arc::downgrade(provider),
            state: Mutex::new(State::default()),
        })
    }

    pub fn invalidate(&self) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("delivery retirement state poisoned"))?;
        anyhow::ensure!(
            !state.held && !state.retired,
            "delivery owner is held or retired"
        );
        state.drained = None;
        state.closed = false;
        Ok(())
    }

    /// Preserve ordinary shutdown behavior, but record drain only when the
    /// already-loaded ledger and actual transaction were healthy before closing.
    pub async fn close(&self, runner: &mut DeliveryRunner) -> anyhow::Result<()> {
        self.invalidate()?;
        let drained = if runner.transaction.recovery_required() {
            None
        } else {
            runner.ledger.as_ref().map(|ledger| {
                ledger.streams.iter().all(|stream| {
                    stream.receipts.back().is_some_and(|latest| {
                        latest.completed == latest.operations && stream.handled == latest.sequence
                    })
                })
            })
        };
        runner.shutdown().await?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("delivery retirement state poisoned"))?;
        state.drained = drained;
        state.closed = true;
        Ok(())
    }

    pub(crate) fn freeze(
        self: &Arc<Self>,
        provider: &Arc<dyn ComputationIndexProvider>,
        allow_pending: bool,
    ) -> anyhow::Result<Option<DeliveryRetirement>> {
        if !self.provider.ptr_eq(&Arc::downgrade(provider)) {
            return Ok(None);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("delivery retirement state poisoned"))?;
        anyhow::ensure!(
            !state.held && !state.retired,
            "delivery owner is held or retired"
        );
        anyhow::ensure!(
            state.closed && (allow_pending || state.drained == Some(true)),
            "delivery owner has pending or unverified completion"
        );
        state.held = true;
        Ok(Some(DeliveryRetirement {
            state: self.clone(),
            resolved: false,
        }))
    }
}

pub(crate) struct DeliveryRetirement {
    state: Arc<DeliveryRetirementState>,
    resolved: bool,
}

impl DeliveryRetirement {
    pub(crate) fn resume(mut self) {
        self.resolved = true;
    }
    pub(crate) fn retire(self) {
        drop(self);
    }
}

impl Drop for DeliveryRetirement {
    fn drop(&mut self) {
        let mut state = self.state.state.lock().unwrap_or_else(|error| {
            log::error!("Fencing poisoned delivery retirement state");
            error.into_inner()
        });
        state.held = false;
        if !self.resolved {
            state.retired = true;
        }
    }
}
