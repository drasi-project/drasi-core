// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

#[cfg(feature = "test-support")]
pub(super) struct QueryTestWakeup {
    pub(super) control: crate::test_support::QueryTestControl,
    pub(super) pending: Arc<AtomicBool>,
    pub(super) changed: Arc<tokio::sync::Notify>,
}

#[cfg(feature = "test-support")]
#[async_trait]
impl WakeupSource for QueryTestWakeup {
    async fn wait(&self) -> anyhow::Result<()> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.pending.load(Ordering::Acquire) {
                return Ok(());
            }
            tokio::select! {
                _ = self.control.notified() => return Ok(()),
                _ = changed => {},
            }
        }
    }

    async fn has_pending(&self) -> anyhow::Result<bool> {
        Ok(true)
    }
}

pub(super) const DELIVERED: &str = "\0computation:query-delivered:v1";
pub(super) const DELIVERY_SEQUENCE: &str = "\0computation:query-delivery-sequence:v1";

pub(super) struct QueryDeliveryWakeup {
    pub(super) scheduled: Option<FutureWakeup>,
    pub(super) pending: Arc<AtomicBool>,
    pub(super) changed: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl WakeupSource for QueryDeliveryWakeup {
    async fn wait(&self) -> anyhow::Result<()> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.pending.load(Ordering::Acquire) {
                return Ok(());
            }
            match &self.scheduled {
                Some(scheduled) => tokio::select! {
                    result = scheduled.wait() => return result,
                    _ = notified => {},
                },
                None => notified.await,
            }
        }
    }
    async fn has_pending(&self) -> anyhow::Result<bool> {
        Ok(self.pending.load(Ordering::Acquire)
            || match &self.scheduled {
                Some(scheduled) => scheduled.has_pending().await?,
                None => false,
            })
    }
}

impl ContinuousQueryTransformer {
    pub(crate) fn track_delivery(mut self) -> Self {
        self.delivery_tracking = true;
        self.configure_output_binding_state();
        self
    }

    pub(super) fn configure_output_binding_state(&mut self) {
        if self.durable_delivery() && self.output_binding_state.is_none() {
            self.output_binding_state = Some(
                super::super::output_bindings::OutputBindingState::new(self.failure.clone()),
            );
        }
    }

    fn durable_delivery(&self) -> bool {
        self.delivery_tracking && self.output_persistent
    }

    fn next_delivery_sequence(&self) -> anyhow::Result<u64> {
        self.delivery_sequence
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("query delivery sequence exhausted"))
    }

    pub(super) async fn load_delivery_bindings(
        &self,
    ) -> anyhow::Result<(super::super::OutputBindings, Option<SourceCheckpoint>)> {
        let store = self
            .query()?
            .resources()
            .checkpoint_store()
            .expect("persistent output");
        let delivered = store.read_checkpoint(DELIVERED).await?;
        let bindings = super::super::OutputBindings::load(
            store
                .read_checkpoint(super::super::output_bindings::CHECKPOINT)
                .await?
                .as_ref(),
        )?;
        bindings.validate_progress(
            delivered
                .as_ref()
                .and_then(|saved| saved.source_position.as_ref()),
        )?;
        if !self.durable_delivery() && !bindings.destinations().is_empty() {
            return Err(super::super::OutputBindingError::Invalid(
                "saved output destinations require tracked delivery".into(),
            )
            .into());
        }
        Ok((bindings, delivered))
    }

    pub(super) async fn prepare_delivery(&mut self) -> anyhow::Result<()> {
        self.pending_output.clear();
        if self.durable_delivery() {
            let query = self.query()?;
            self.output_binding_state
                .as_ref()
                .expect("durable delivery bindings")
                .bind_standalone_owner(&self.provider, query.retirement_owner())?;
            let store = query
                .resources()
                .checkpoint_store()
                .expect("persistent output");
            let head = self.results.snapshot()?.as_of_sequence;
            let (bindings, delivered) = self.load_delivery_bindings().await?;
            let emission = store.read_checkpoint(DELIVERY_SEQUENCE).await?;
            if delivered
                .as_ref()
                .is_some_and(|saved| saved.sequence > head)
                || emission
                    .as_ref()
                    .is_some_and(|saved| saved.source_position.is_some())
            {
                anyhow::bail!("invalid query delivery checkpoint");
            }
            if delivered.is_none() && head != 0 {
                anyhow::bail!("query output has no durable handoff checkpoint");
            }
            let delivered = delivered.map_or(0, |saved| saved.sequence);
            let emission = emission.map_or(0, |saved| saved.sequence);
            self.delivery_sequence.fetch_max(emission, Ordering::AcqRel);
            self.delivered_output.store(delivered, Ordering::Release);
            query
                .resource_transaction(|| async {
                    store
                        .stage_checkpoint(DELIVERED, delivered, bindings.fingerprint())
                        .await
                })
                .await?;
            self.output_binding_state
                .as_ref()
                .expect("durable delivery bindings")
                .publish(bindings.clone(), head > delivered)?;
            self.output_bindings = bindings;
            self.resume_pending_delivery()?;
        }
        self.replay_pending
            .store(!self.pending_output.is_empty(), Ordering::Release);
        self.delivery_changed.notify_waiters();
        Ok(())
    }

    pub(super) async fn bind_delivery_destinations(
        &mut self,
        bindings: &super::super::OutputBindings,
    ) -> anyhow::Result<()> {
        if !self.durable_delivery() {
            anyhow::ensure!(
                bindings.destinations().is_empty(),
                "query does not track persistent output delivery"
            );
            return Ok(());
        }
        if self.failure.load(Ordering::Acquire)
            || !self
                .results
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("query output state poisoned"))?
                .ready
        {
            return Err(super::super::OutputBindingError::RecoveryRequired.into());
        }
        bindings.validate_query(self.query()?)?;
        anyhow::ensure!(
            !bindings.has_shared() || self.options.publication == QueryPublicationMode::Atomic,
            "shared QoS requires atomic query publication"
        );
        let pending =
            self.results.snapshot()?.as_of_sequence > self.delivered_output.load(Ordering::Acquire);
        self.output_bindings.validate_change(bindings, pending)?;
        if &self.output_bindings == bindings {
            self.output_bindings = bindings.clone();
            return self.resume_pending_delivery();
        }
        let mut guard = ProcessingGuard {
            failure: self.failure.clone(),
            complete: false,
            progress: self.source_progress.clone(),
        };
        let bytes = bindings.encode()?;
        let query = self.query()?;
        let checkpoint = query
            .resources()
            .checkpoint_store()
            .expect("persistent output");
        query
            .resource_transaction(|| async {
                checkpoint
                    .stage_checkpoint(super::super::output_bindings::CHECKPOINT, 1, Some(&bytes))
                    .await?;
                checkpoint
                    .stage_checkpoint(
                        DELIVERED,
                        self.delivered_output.load(Ordering::Acquire),
                        bindings.fingerprint(),
                    )
                    .await
            })
            .await?;
        self.output_binding_state
            .as_ref()
            .expect("durable delivery bindings")
            .publish(bindings.clone(), pending)?;
        self.output_bindings = bindings.clone();
        self.resume_pending_delivery()?;
        guard.complete = true;
        Ok(())
    }

    fn resume_pending_delivery(&mut self) -> anyhow::Result<()> {
        let delivered = self.delivered_output.load(Ordering::Acquire);
        let state = self
            .results
            .state
            .read()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
        let pending: VecDeque<_> = state
            .outbox
            .iter()
            .filter(|envelope| envelope.system().sequence() > delivered)
            .cloned()
            .collect();
        if state.sequence > delivered
            && pending.front().map(|envelope| envelope.system().sequence())
                != delivered.checked_add(1)
        {
            anyhow::bail!("query handoff history was retired before durable acceptance");
        }
        drop(state);
        self.pending_output = pending;
        self.replay_pending
            .store(!self.pending_output.is_empty(), Ordering::Release);
        self.delivery_changed.notify_waiters();
        Ok(())
    }

    pub(super) async fn stage_delivery(&self) -> Result<(), IndexError> {
        if self.durable_delivery() {
            let sequence = self
                .next_delivery_sequence()
                .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?;
            self.query()
                .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?
                .resources()
                .checkpoint_store()
                .expect("persistent output")
                .stage_checkpoint(DELIVERY_SEQUENCE, sequence, None)
                .await?;
        }
        Ok(())
    }

    pub(super) fn live_delivery(
        &self,
        envelope: super::super::ChangeEnvelope,
    ) -> anyhow::Result<super::super::ChangeEnvelope> {
        if !self.delivery_tracking {
            return Ok(envelope);
        }
        let sequence = self.next_delivery_sequence()?;
        let delivery = envelope.reemit(sequence)?;
        self.delivery_sequence.store(sequence, Ordering::Release);
        if let Some(bindings) = &self.output_binding_state {
            bindings.pending(true);
        }
        Ok(delivery)
    }

    pub(super) fn shared_delivery_mutations(
        &self,
        reservations: super::super::output_bindings::OutputReservations,
        envelope: Option<&super::super::ChangeEnvelope>,
    ) -> Result<Vec<Box<dyn drasi_core::computation::TransactionGroupMutation>>, IndexError> {
        let prepare = || -> anyhow::Result<_> {
            let delivery = envelope
                .map(|envelope| -> anyhow::Result<_> {
                    Ok(envelope.reemit(self.next_delivery_sequence()?)?)
                })
                .transpose()?;
            reservations.mutations(delivery.as_ref())
        };
        prepare().map_err(super::super::output_bindings::index_error)
    }

    pub(super) async fn replay_output(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        let Some(envelope) = self.pending_output.front().cloned() else {
            return Ok(Vec::new());
        };
        let sequence = self.next_delivery_sequence()?;
        let query = self.query()?;
        let checkpoint = query
            .resources()
            .checkpoint_store()
            .expect("persistent replay");
        query
            .resource_transaction(|| async {
                checkpoint
                    .stage_checkpoint(DELIVERY_SEQUENCE, sequence, None)
                    .await
            })
            .await?;
        let delivery = envelope.reemit(sequence)?;
        self.delivery_sequence.store(sequence, Ordering::Release);
        self.pending_output.pop_front();
        self.replay_pending
            .store(!self.pending_output.is_empty(), Ordering::Release);
        Ok(vec![OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: delivery,
        }])
    }

    pub(super) async fn confirm_delivery(
        &mut self,
        outputs: &[OutputEnvelope],
    ) -> anyhow::Result<()> {
        if !self.durable_delivery() || outputs.is_empty() {
            return Ok(());
        }
        Box::pin(self.confirm_durable_delivery(outputs)).await
    }

    async fn confirm_durable_delivery(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        let mut confirmed = self.delivered_output.load(Ordering::Acquire);
        for output in outputs {
            let sequence = QueryChangeCodec::query_sequence(&output.envelope)?;
            anyhow::ensure!(
                output.port.as_str() == "out"
                    && output.envelope.system().stream() == &self.definition.output_stream
                    && super::super::QueryRecoveryIdentity::from_envelope(&output.envelope)?
                        == self.recovery_identity()?
                    && QueryChangeCodec::query_generation(&output.envelope)?
                        == self.results.snapshot()?.generation,
                "query delivery confirmation belongs to another output lifetime"
            );
            if sequence > confirmed {
                anyhow::ensure!(
                    confirmed.checked_add(1) == Some(sequence)
                        && sequence <= self.results.snapshot()?.as_of_sequence,
                    "query delivery confirmation would skip pending output"
                );
                confirmed = sequence;
            }
        }
        let query = self.query()?;
        let checkpoint = query
            .resources()
            .checkpoint_store()
            .expect("persistent output");
        query
            .resource_transaction(|| async {
                checkpoint
                    .stage_checkpoint(DELIVERED, confirmed, self.output_bindings.fingerprint())
                    .await
            })
            .await?;
        self.delivered_output.store(confirmed, Ordering::Release);
        if let Some(bindings) = &self.output_binding_state {
            bindings.pending(self.results.snapshot()?.as_of_sequence > confirmed);
        }
        Ok(())
    }
}
