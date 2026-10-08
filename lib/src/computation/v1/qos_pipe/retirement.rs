// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

pub(crate) struct QosRetirement {
    channel: Arc<QosChannel>,
    transaction: Option<drasi_core::computation::ComputationTransactionRetirement>,
}

impl QosChannel {
    pub(crate) async fn freeze_for_retirement(
        self: &Arc<Self>,
        allow_pending: bool,
    ) -> Result<QosRetirement, PipeError> {
        // Writers take journal state before the standalone transaction gate.
        // Inspect while holding both; no subsequent proof read may wait on state.
        let state = self.state.lock().await;
        self.check()?;
        if self.shared.is_some() {
            return Err(backend(
                "shared journals require their group's retirement gate",
            ));
        }
        let storage = self
            .storage()?
            .ok_or_else(|| backend("retirement requires persistent journal storage"))?;
        let transaction = storage
            .transaction
            .freeze_for_retirement()
            .map_err(|error| PipeError::Backend(error.into()))?;
        if !allow_pending
            && state
                .metadata
                .cursors
                .values()
                .any(|cursor| !cursor.retired && cursor.position != state.metadata.head)
        {
            transaction.resume();
            return Err(backend(
                "journal still has unhandled subscriber obligations",
            ));
        }
        Ok(QosRetirement {
            channel: self.clone(),
            transaction: Some(transaction),
        })
    }
}

impl QosRetirement {
    pub(crate) fn resume(mut self) {
        if let Some(transaction) = self.transaction.take() {
            transaction.resume();
        }
    }
    pub(crate) fn retire(mut self) {
        if let Some(transaction) = self.transaction.take() {
            transaction.retire();
        }
    }
}

impl Drop for QosRetirement {
    fn drop(&mut self) {
        // Fence the transaction before waking capacity waiters on abandonment.
        drop(self.transaction.take());
        self.channel.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::computation::ComputationIndexProvider;
    use drasi_index_rocksdb::RocksDbIndexProvider;

    #[tokio::test(flavor = "current_thread")]
    async fn standalone_retirement_resolves_waiting_writers_and_receivers() {
        for resolution in ["resume", "retire", "abandon"] {
            let directory = tempfile::tempdir().unwrap();
            let indexes = LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(
                directory.path(),
                false,
                false,
            )))
            .create_indexes("retirement", "journal")
            .await
            .unwrap();
            let mut codec = EnvelopeCodec::new(NonZeroUsize::new(1 << 20).unwrap());
            codec.register_schema(GraphChangeCodec::schema()).unwrap();
            let channel = QosChannel::persistent(
                QosChannelDefinition {
                    stream: StreamId::try_new("test/out").unwrap(),
                    capacity: NonZeroUsize::new(2).unwrap(),
                    durable: true,
                    retention: RetentionPolicy::Backpressure,
                    subscribers: BTreeMap::from([("consumer".into(), SubscriptionStart::Earliest)]),
                },
                indexes,
                codec,
                "journal",
            )
            .await
            .unwrap();
            let frozen = channel.freeze_for_retirement(false).await.unwrap();
            let mut pipe = channel.bind("consumer", ReplayGapPolicy::Strict).unwrap();
            let mut receiver = pipe.pipe.take_receiver().unwrap();
            let mut read = Box::pin(receiver.receive());
            assert!(futures::poll!(&mut read).is_pending());
            let event = super::super::super::pipe::test_envelope(1);
            let mut write = Box::pin(channel.publish(&event));
            assert!(futures::poll!(&mut write).is_pending());
            match resolution {
                "resume" => frozen.resume(),
                "retire" => frozen.retire(),
                _ => drop(frozen),
            }
            let (written, received) =
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    tokio::join!(write, read)
                })
                .await
                .unwrap();
            if resolution == "resume" {
                written.unwrap();
                received
                    .unwrap()
                    .unwrap()
                    .into_parts()
                    .1
                    .unwrap()
                    .complete(HandlingOutcome::Handled)
                    .await
                    .unwrap();
                assert_eq!(channel.progress().await.unwrap().processed["consumer"], 1);
            } else {
                assert!(written.is_err());
                assert!(received.is_err());
                assert!(channel.freeze_for_retirement(false).await.is_err());
            }
            pipe.control.cancel();
            channel.shutdown().await.unwrap();
        }
    }
}
