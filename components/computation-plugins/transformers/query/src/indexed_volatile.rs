// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

impl DurableQuery {
    pub(super) async fn start_volatile(
        &mut self,
        processing: &NativeProcessing,
        definition: Arc<ContinuousQueryDefinition>,
        execution: Arc<QueryExecutionSettings>,
        middleware: Arc<drasi_core::middleware::MiddlewareTypeRegistry>,
        force_reset: bool,
    ) -> Result<()> {
        let updating =
            self.update_operation.is_some() && (!self.initialized || !self.bootstrap_complete);
        let reset = force_reset || self.initialized && !self.bootstrap_complete || updating;
        if reset {
            anyhow::ensure!(
                updating
                    || self.bootstrap_configured && self.recovery == QueryRecoveryPolicy::AutoReset,
                "volatile reset requires an accepted update or AutoReset with bootstrap"
            );
            anyhow::ensure!(
                self.pending.is_none(),
                "volatile reset cannot discard unconfirmed output"
            );
        }
        let existing = (!reset).then(|| self.evaluator.clone()).flatten();
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
        let indexes = self.indexes.clone();
        let configuration = self.configuration.clone();
        let bootstrap_configured = self.bootstrap_configured;
        let expected_sequence = self.sequence;
        let expected_count = if initialize {
            0
        } else {
            self.output.row_count()?
        };
        let initialized = Arc::new(Mutex::new(None));
        let result = initialized.clone();
        let (generation, complete) = processing
            .run(move |context| async move {
                let context = Arc::new(context);
                let _scope = indexes.bind(context.clone())?;
                let profile = context.state_info().await?;
                anyhow::ensure!(
                    profile.volatile && !profile.persistent_checkpoints,
                    "volatile query requires the actual volatile processing profile"
                );
                if reset {
                    context.reset_volatile_state(true).await?;
                }
                let complete = if initialize {
                    anyhow::ensure!(
                        context.read_checkpoint(CONFIGURATION).await?.is_none(),
                        "volatile query cannot adopt another instance's state"
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
                        .stage_bootstrap_complete(!bootstrap_configured)
                        .await?;
                    !bootstrap_configured
                } else {
                    let saved = context
                        .read_checkpoint(CONFIGURATION)
                        .await?
                        .context("volatile query lost its actual index resources")?;
                    anyhow::ensure!(
                        saved.sequence == 1
                            && saved.source_position.as_deref() == Some(configuration.as_slice())
                            && context.read_result_sequence().await? == Some(expected_sequence)
                            && number(&context, ROW_COUNT).await? == expected_count,
                        "volatile query state changed while stopped"
                    );
                    let complete = number(&context, BOOTSTRAP_COMPLETE_CHECKPOINT).await?;
                    anyhow::ensure!(complete <= 1, "invalid volatile bootstrap marker");
                    complete == 1
                };
                let query = match existing {
                    Some(query) => query,
                    None => Arc::new(
                        evaluator(
                            &definition.query,
                            definition.language,
                            &execution,
                            Some(indexes),
                            middleware,
                        )
                        .await?,
                    ),
                };
                context.recover_input_progress().await?;
                let generation = generation.max(context.reset_generation().await?.unwrap_or(0));
                *result
                    .lock()
                    .map_err(|_| anyhow::anyhow!("volatile evaluator construction poisoned"))? =
                    Some(query);
                Ok((generation, complete))
            })
            .await?;
        self.evaluator = initialized
            .lock()
            .map_err(|_| anyhow::anyhow!("volatile evaluator construction poisoned"))?
            .take();
        anyhow::ensure!(
            self.evaluator.is_some(),
            "volatile evaluator construction did not complete"
        );
        if let Some((producer, _)) = next_epoch {
            self.producer = producer;
            self.sequence = 0;
            self.delivered = 0;
            self.pending = None;
            self.output.restore(
                self.producer.clone(),
                0,
                Rows::new(),
                false,
                Vec::new(),
                generation,
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
