// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{OperationGuard, State, MAX_OPERATIONS};
use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_computation_contracts::{
    BootstrapState, BOOTSTRAP_STATE_CHECKPOINT, MAX_BOOTSTRAP_STATE_BYTES,
};
use drasi_core::{computation::ComputationTransaction, interface::StorageDurability};
use std::sync::{Arc, Weak};
use tokio_util::sync::CancellationToken;

pub(super) struct ProcessingBootstrapState {
    state: Weak<State>,
}

impl ProcessingBootstrapState {
    pub(super) fn new(state: Weak<State>) -> Self {
        Self { state }
    }

    fn acquire(
        &self,
    ) -> Result<(
        OperationGuard,
        Arc<ComputationTransaction>,
        CancellationToken,
    )> {
        let state = self
            .state
            .upgrade()
            .context("bootstrap processing owner was dropped")?;
        let (owner, cancel) = {
            let mut live = state
                .live
                .lock()
                .map_err(|_| anyhow::anyhow!("bootstrap processing state poisoned"))?;
            anyhow::ensure!(live.active, "bootstrap processing owner is not active");
            anyhow::ensure!(
                live.outstanding < MAX_OPERATIONS,
                "bootstrap processing operation capacity exceeded"
            );
            let owner = live
                .owner
                .clone()
                .context("bootstrap processing owner is missing")?;
            if !state
                .progress
                .as_ref()
                .is_some_and(|progress| progress.is_volatile())
            {
                owner.resources().atomic_result_transaction()?;
            }
            state.checkpoint_store(owner.resources())?;
            live.outstanding += 1;
            (owner, live.cancel.clone())
        };
        Ok((OperationGuard(state), owner, cancel))
    }

    fn validate(bytes: &Bytes) -> Result<()> {
        anyhow::ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_BOOTSTRAP_STATE_BYTES,
            "bootstrap state must contain 1..={MAX_BOOTSTRAP_STATE_BYTES} bytes"
        );
        Ok(())
    }
}

#[async_trait]
impl BootstrapState for ProcessingBootstrapState {
    fn durability(&self) -> StorageDurability {
        let Some(state) = self.state.upgrade() else {
            return StorageDurability::UNKNOWN;
        };
        let durability = match state.live.lock() {
            Ok(live) if live.active => live
                .owner
                .as_ref()
                .map_or(StorageDurability::UNKNOWN, |owner| {
                    owner.resources().durability()
                }),
            Ok(_) => StorageDurability::UNKNOWN,
            Err(error) => {
                log::error!("bootstrap processing durability state poisoned: {error}");
                StorageDurability::UNKNOWN
            }
        };
        durability
    }

    async fn read(&self) -> Result<Option<Bytes>> {
        let (_guard, owner, cancel) = self.acquire()?;
        let store = _guard.0.checkpoint_store(owner.resources())?;
        let saved = tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("bootstrap state read was cancelled"),
            result = owner.run(async { Ok(store.read_checkpoint(BOOTSTRAP_STATE_CHECKPOINT).await?) }) => result?,
        };
        saved
            .map(|saved| {
                anyhow::ensure!(saved.sequence == 1, "invalid bootstrap state version");
                let bytes = saved
                    .source_position
                    .context("bootstrap state payload is missing")?;
                Self::validate(&bytes)?;
                Ok(bytes)
            })
            .transpose()
    }

    async fn write(&self, bytes: Bytes) -> Result<()> {
        Self::validate(&bytes)?;
        let (_guard, owner, cancel) = self.acquire()?;
        let store = _guard.0.checkpoint_store(owner.resources())?;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("bootstrap state write was cancelled"),
            result = owner.run(async {
                store.stage_checkpoint(BOOTSTRAP_STATE_CHECKPOINT, 1, Some(&bytes)).await?;
                Ok(())
            }) => result?,
        };
        Ok(())
    }
}
