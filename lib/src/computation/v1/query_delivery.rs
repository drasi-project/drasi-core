// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

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
        self
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

    pub(super) async fn prepare_delivery(&mut self) -> anyhow::Result<()> {
        self.pending_output.clear();
        if self.durable_delivery() {
            let query = self.query()?;
            let store = query
                .resources()
                .checkpoint_store()
                .expect("persistent output");
            let head = self.results.snapshot()?.as_of_sequence;
            let delivered = store.read_checkpoint(DELIVERED).await?;
            let emission = store.read_checkpoint(DELIVERY_SEQUENCE).await?;
            if delivered
                .as_ref()
                .is_some_and(|saved| saved.source_position.is_some() || saved.sequence > head)
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
                    store.stage_checkpoint(DELIVERED, delivered, None).await
                })
                .await?;
            let state = self
                .results
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
            let pending: Vec<_> = state
                .outbox
                .iter()
                .filter(|envelope| envelope.system().sequence() > delivered)
                .cloned()
                .collect();
            if head > delivered
                && pending.first().map(|envelope| envelope.system().sequence())
                    != delivered.checked_add(1)
            {
                anyhow::bail!("query handoff history was retired before durable acceptance");
            }
            drop(state);
            self.pending_output.extend(pending);
        }
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
        Ok(delivery)
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
                    .stage_checkpoint(DELIVERED, confirmed, None)
                    .await
            })
            .await?;
        self.delivered_output.store(confirmed, Ordering::Release);
        Ok(())
    }
}
