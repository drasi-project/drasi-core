// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::{
    QueryOutputObservation, QueryOutputView, QueryRecoveryHead, QueryRecoveryReader,
    QueryRecoveryView, QueryViewProvider,
};
use drasi_computation_plugin_sdk::output_reads::WireQueryObservation;

impl NativeQueryProvider {
    pub(super) fn require_recovery(&self) -> Result<(), OutputReadError> {
        if !self.recovery_views {
            return Err(invalid("query recovery views were not negotiated"));
        }
        if self.reader.fence.retired.load(Ordering::Acquire) {
            return Err(OutputReadError::Retired);
        }
        if !self.reader.fence.recovery_ready.load(Ordering::Acquire) {
            return Err(OutputReadError::NotReady(
                "query activation or cleanup is incomplete".into(),
            ));
        }
        Ok(())
    }

    pub(super) async fn read_observation(
        &self,
        code: u32,
    ) -> Result<QueryOutputObservation, OutputReadError> {
        let bytes = self.reader.read(code, &[]).await?;
        let wire: WireQueryObservation = self.reader.outcome(&bytes)?;
        let observation = wire.into_observation()?;
        observation.validate(&self.reader.endpoint.component, &self.description)?;
        Ok(observation)
    }

    pub(super) async fn read_view(
        &self,
        code: u32,
    ) -> Result<(OutputCursor, QueryOutputView), OutputReadError> {
        match self.reader.transfer(code, &[], None).await? {
            transfer::Read::QueryView {
                snapshot,
                retained,
                generation,
                config_hash,
            } => self.view(snapshot, retained, generation, config_hash),
            _ => Err(invalid(
                "native query view transfer returned another read kind",
            )),
        }
    }
}

#[async_trait]
impl QueryRecoveryReader for NativeQueryProvider {
    async fn recovery_head(&self) -> Result<QueryRecoveryHead, OutputReadError> {
        let observation = self.query_recovery_observation().await?;
        Ok(QueryRecoveryHead {
            identity: QueryRecoveryIdentity::for_producer(
                observation.cursor.producer(),
                observation.config_hash,
            )
            .map_err(invalid)?,
            sequence: observation.cursor.sequence(),
            generation: observation.generation,
        })
    }

    async fn recovery_view(
        &self,
        after: Option<u64>,
    ) -> Result<QueryRecoveryView, OutputReadError> {
        let (cursor, view) = self.query_recovery_view().await?;
        Ok(QueryRecoveryView {
            identity: QueryRecoveryIdentity::for_producer(cursor.producer(), view.config_hash)
                .map_err(invalid)?,
            snapshot: view.snapshot,
            oldest_sequence: view
                .retained
                .first()
                .map(|output| output.system().sequence()),
            retained: after.map(|after| {
                view.retained
                    .into_iter()
                    .filter(|output| output.system().sequence() > after)
                    .collect()
            }),
        })
    }

    async fn wait_recovery(
        &self,
        after: Option<&QueryRecoveryHead>,
    ) -> Result<QueryRecoveryHead, OutputReadError> {
        let mut changed = self.reader.fence.recovery_changed.subscribe();
        loop {
            match self.recovery_head().await {
                Ok(head) if after != Some(&head) => return Ok(head),
                Ok(_) | Err(OutputReadError::NotReady(_)) => {}
                Err(OutputReadError::Retired)
                    if !self.reader.fence.retired.load(Ordering::Acquire) => {}
                Err(error) => return Err(error),
            }
            changed
                .changed()
                .await
                .map_err(|_| OutputReadError::Retired)?;
        }
    }
}
