// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The volatile profile: state lives in memory and in the host's volatile
//! stores, and each initialization publishes a new producer epoch.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use drasi_computation_contracts::{
    QueryRecoveryPolicy as RecoveryPolicy, BOOTSTRAP_COMPLETE_CHECKPOINT,
};

use super::{
    engine::{
        bound, first_input_boundary, number, Engine, FirstInput, CONFIGURATION, EMISSION, PRODUCER,
        ROW_COUNT, VOLATILE_DELIVERED,
    },
    Rows, TransformerLogic,
};
use crate::NativeProcessing;

impl<L: TransformerLogic> Engine<L> {
    pub(super) async fn start_volatile(
        &mut self,
        processing: &NativeProcessing,
        force_reset: bool,
    ) -> Result<()> {
        let updating =
            self.update_operation.is_some() && (!self.initialized || !self.bootstrap_complete);
        let reset = force_reset || self.initialized && !self.bootstrap_complete || updating;
        if reset {
            anyhow::ensure!(
                !self.unconnected_inputs,
                "cannot rebuild an unconnected transformer from a missing input"
            );
            anyhow::ensure!(
                updating || self.bootstrap_configured && self.recovery == RecoveryPolicy::AutoReset,
                "volatile reset requires an accepted update or AutoReset with bootstrap"
            );
            anyhow::ensure!(
                self.pending.is_none(),
                "volatile reset cannot discard unconfirmed output"
            );
        }
        let existing = (!reset).then(|| self.state.clone()).flatten();
        anyhow::ensure!(
            !self.initialized || existing.is_some() || reset,
            "volatile state requires explicit reconstruction and bootstrap"
        );
        let initialize = existing.is_none();
        let next_epoch = initialize
            .then(|| self.output.next_volatile_epoch())
            .transpose()?;
        let candidate = next_epoch
            .as_ref()
            .map_or_else(|| self.producer.clone(), |(producer, _)| producer.clone());
        let generation = next_epoch.as_ref().map_or(0, |(_, generation)| *generation);
        let logic = self.logic.clone();
        let configuration = self.configuration.clone();
        let bootstrap_configured = self.bootstrap_configured;
        let unconnected = self.unconnected_inputs;
        let output = self.output.clone();
        let expected_sequence = self.sequence;
        let expected_count = if initialize {
            0
        } else {
            self.output.row_count()?
        };
        let opened = Arc::new(Mutex::new(None));
        let result = opened.clone();
        let (generation, complete, connected_epoch) = processing
            .run(move |context| async move {
                let (context, _binding) = bound(&*logic, context)?;
                let profile = context.state_info().await?;
                anyhow::ensure!(
                    profile.volatile && !profile.persistent_checkpoints,
                    "volatile transformer requires the actual volatile processing profile"
                );
                if reset {
                    context.reset_volatile_state(true).await?;
                }
                let complete = if initialize {
                    anyhow::ensure!(
                        context.read_checkpoint(CONFIGURATION).await?.is_none(),
                        "volatile transformer cannot adopt another instance's state"
                    );
                    context
                        .stage_checkpoint(CONFIGURATION, 1, Some(&configuration))
                        .await?;
                    context
                        .stage_checkpoint(PRODUCER, 1, Some(&serde_json::to_vec(&candidate)?))
                        .await?;
                    for key in [EMISSION, ROW_COUNT, VOLATILE_DELIVERED] {
                        context.stage_checkpoint(key, 0, None).await?;
                    }
                    context.stage_result_sequence(0).await?;
                    context
                        .stage_bootstrap_complete(!bootstrap_configured || unconnected)
                        .await?;
                    !bootstrap_configured || unconnected
                } else {
                    let saved = context
                        .read_checkpoint(CONFIGURATION)
                        .await?
                        .context("volatile transformer lost its actual state resources")?;
                    anyhow::ensure!(
                        saved.sequence == 1
                            && saved.source_position.as_deref() == Some(configuration.as_slice())
                            && context.read_result_sequence().await? == Some(expected_sequence)
                            && number(&context, ROW_COUNT).await? == expected_count,
                        "volatile transformer state changed while stopped"
                    );
                    let complete = number(&context, BOOTSTRAP_COMPLETE_CHECKPOINT).await?;
                    anyhow::ensure!(complete <= 1, "invalid volatile bootstrap marker");
                    complete == 1
                };
                let connected_epoch = if first_input_boundary(
                    &context,
                    bootstrap_configured,
                    unconnected,
                    initialize,
                    true,
                    false,
                )
                .await?
                    == FirstInput::Initialize
                {
                    let epoch = output.next_volatile_epoch()?;
                    context
                        .stage_checkpoint(PRODUCER, 1, Some(&serde_json::to_vec(&epoch.0)?))
                        .await?;
                    Some(epoch)
                } else {
                    None
                };
                let state = match existing {
                    Some(state) => state,
                    None => logic.open(&context).await?,
                };
                context.recover_input_progress().await?;
                let generation = connected_epoch
                    .as_ref()
                    .map_or(generation, |(_, g)| *g)
                    .max(context.reset_generation().await?.unwrap_or(0));
                *result
                    .lock()
                    .map_err(|_| anyhow::anyhow!("volatile state construction poisoned"))? =
                    Some(state);
                Ok((
                    generation,
                    complete && connected_epoch.is_none(),
                    connected_epoch,
                ))
            })
            .await?;
        self.state = opened
            .lock()
            .map_err(|_| anyhow::anyhow!("volatile state construction poisoned"))?
            .take();
        anyhow::ensure!(
            self.state.is_some(),
            "volatile state construction did not complete"
        );
        if let Some((producer, _)) = connected_epoch.or(next_epoch) {
            self.producer = producer;
            self.sequence = 0;
            self.delivered = 0;
            self.pending = None;
            let logic = self.logic.clone();
            self.output.restore(
                self.producer.clone(),
                0,
                Rows::new(),
                false,
                Vec::new(),
                generation,
                &|envelope, producer, generation| logic.verify(envelope, producer, generation),
            )?;
        }
        self.initialized = true;
        self.bootstrap_complete = complete;
        self.replay = self.pending.is_some();
        if complete {
            self.refresh_schedule(processing).await?;
        }
        Ok(())
    }
}
