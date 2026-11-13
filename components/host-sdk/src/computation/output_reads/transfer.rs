// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_sdk::output_reads::{
    WireReadMetadata, WireReadPage, WireReplayMetadata, WireSnapshotMetadata,
};
#[cfg(test)]
mod tests;

pub(super) enum Read {
    Snapshot(OutputSnapshot),
    Replay(OutputReplay),
    QuerySnapshot(OutputSnapshot, u64),
    QueryReplay(OutputReplay, u64),
    QueryView {
        snapshot: OutputSnapshot,
        retained: OutputReplay,
        generation: u64,
        config_hash: u64,
    },
}

struct Content {
    metadata: WireReadMetadata,
    rows: im::HashMap<RecordId, Record>,
    envelopes: Vec<ChangeEnvelope>,
    row_count: usize,
    envelope_count: usize,
    index: u64,
}

impl Content {
    fn new(
        context: &super::Context,
        metadata: WireReadMetadata,
        code: u32,
        after: Option<&OutputCursor>,
    ) -> Result<Self, OutputReadError> {
        let (operation, snapshot, replay) = match &metadata {
            WireReadMetadata::Snapshot(snapshot) => {
                (ext::operation::SNAPSHOT, Some(snapshot), None)
            }
            WireReadMetadata::Replay(replay) => (ext::operation::REPLAY_AFTER, None, Some(replay)),
            WireReadMetadata::QuerySnapshot { snapshot, .. } => {
                (ext::operation::QUERY_SNAPSHOT, Some(snapshot), None)
            }
            WireReadMetadata::QueryReplay { replay, .. } => {
                (ext::operation::QUERY_REPLAY, None, Some(replay))
            }
            WireReadMetadata::QueryView {
                snapshot, retained, ..
            } => {
                if snapshot.cursor != retained.head {
                    return Err(invalid(
                        "query transfer snapshot and history have different heads",
                    ));
                }
                (ext::operation::QUERY_VIEW, Some(snapshot), Some(retained))
            }
        };
        if code != operation
            && !(code == ext::operation::QUERY_RECOVERY_VIEW
                && operation == ext::operation::QUERY_VIEW)
        {
            return Err(invalid("native transfer answered another read operation"));
        }
        if let Some(snapshot) = snapshot {
            context.check_cursor(&snapshot.cursor.clone().into_cursor()?)?;
            if snapshot.schema != *context.schema.descriptor() || snapshot.dataset.is_empty() {
                return Err(invalid(
                    "native transfer has an invalid snapshot schema or dataset",
                ));
            }
        }
        if let Some(replay) = replay {
            let start = replay.after.clone().into_cursor()?;
            let head = replay.head.clone().into_cursor()?;
            context.check_cursor(&head)?;
            start.ensure_same_epoch(&head)?;
            if start.sequence() > head.sequence() || after.is_some_and(|after| after != &start) {
                return Err(invalid("native transfer replay answers another cursor"));
            }
        }
        let row_count = usize::try_from(snapshot.map_or(0, |value| value.records))
            .map_err(|_| invalid("snapshot record count exceeds this host's address space"))?;
        let envelope_count = usize::try_from(replay.map_or(0, |value| value.envelopes))
            .map_err(|_| invalid("replay envelope count exceeds this host's address space"))?;
        Ok(Self {
            metadata,
            rows: im::HashMap::new(),
            envelopes: Vec::new(),
            row_count,
            envelope_count,
            index: 0,
        })
    }

    fn append(
        &mut self,
        context: &super::Context,
        page: WireReadPage<'_>,
    ) -> Result<bool, OutputReadError> {
        self.index = self
            .index
            .checked_add(1)
            .ok_or_else(|| invalid("native transfer page index exhausted"))?;
        if page.index != self.index
            || page.records.len().saturating_add(page.envelopes.len()) > ext::MAX_PAGE_ITEMS
        {
            return Err(invalid(
                "native transfer has an invalid page index or item count",
            ));
        }
        if page.records.len() > self.row_count.saturating_sub(self.rows.len())
            || page.envelopes.len() > self.envelope_count.saturating_sub(self.envelopes.len())
        {
            return Err(invalid(
                "native transfer exceeds its captured record/envelope counts",
            ));
        }
        let progressed = !page.records.is_empty() || !page.envelopes.is_empty();
        for wire in page.records {
            let record = context.record(&wire)?;
            if self
                .rows
                .insert(record.identity().clone(), record)
                .is_some()
            {
                return Err(invalid(
                    "native transfer repeats a snapshot record identity",
                ));
            }
        }
        for frame in page.envelopes {
            self.envelopes.push(context.envelope(&frame.envelope)?);
        }
        let complete =
            self.rows.len() == self.row_count && self.envelopes.len() == self.envelope_count;
        if page.done != complete || !page.done && !progressed {
            return Err(invalid("native transfer ended early or did not advance"));
        }
        Ok(complete)
    }

    fn finish(self) -> Result<Read, OutputReadError> {
        fn snapshot(
            metadata: WireSnapshotMetadata,
            rows: im::HashMap<RecordId, Record>,
        ) -> Result<OutputSnapshot, OutputReadError> {
            OutputSnapshot::try_new(
                metadata.cursor.into_cursor()?,
                metadata.dataset,
                metadata.schema,
                rows,
            )
        }
        fn replay(
            metadata: WireReplayMetadata,
            envelopes: Vec<ChangeEnvelope>,
        ) -> Result<OutputReplay, OutputReadError> {
            OutputReplay::try_new(
                metadata.after.into_cursor()?,
                metadata.head.into_cursor()?,
                envelopes,
            )
        }
        Ok(match self.metadata {
            WireReadMetadata::Snapshot(metadata) => Read::Snapshot(snapshot(metadata, self.rows)?),
            WireReadMetadata::Replay(metadata) => Read::Replay(replay(metadata, self.envelopes)?),
            WireReadMetadata::QuerySnapshot {
                snapshot: metadata,
                generation,
            } => Read::QuerySnapshot(snapshot(metadata, self.rows)?, generation),
            WireReadMetadata::QueryReplay {
                replay: metadata,
                generation,
            } => Read::QueryReplay(replay(metadata, self.envelopes)?, generation),
            WireReadMetadata::QueryView {
                snapshot: rows,
                retained,
                generation,
                config_hash,
            } => Read::QueryView {
                snapshot: snapshot(rows, self.rows)?,
                retained: replay(retained, self.envelopes)?,
                generation,
                config_hash,
            },
        })
    }
}

impl Reader {
    pub(super) async fn transfer(
        &self,
        code: u32,
        input: &[u8],
        after: Option<&OutputCursor>,
    ) -> Result<Read, OutputReadError> {
        let (generation, mut changed) = self.fence.enter()?;
        let handle = self.handle.transfer(code, input).map_err(failure_error)?;
        let opening = handle
            .begin(ext::transfer_operation::START, &[])
            .map_err(failure_error)?;
        self.context
            .receive_transfer(opening, code, after, generation, &mut changed, || {
                handle.begin(ext::transfer_operation::NEXT, &[])
            })
            .await
    }
}

impl super::Context {
    pub(in crate::computation) async fn snapshot_transfer(
        &self,
        opening: OperationFuture,
        generation: u64,
        changed: &mut watch::Receiver<()>,
        next: impl FnMut() -> Result<OperationFuture, Failure> + Send,
    ) -> Result<OutputSnapshot, OutputReadError> {
        match self
            .receive_transfer(
                opening,
                ext::operation::SNAPSHOT,
                None,
                generation,
                changed,
                next,
            )
            .await?
        {
            Read::Snapshot(snapshot) => Ok(snapshot),
            _ => Err(invalid("native follow opening returned another read kind")),
        }
    }

    async fn receive_transfer(
        &self,
        opening: OperationFuture,
        code: u32,
        after: Option<&OutputCursor>,
        generation: u64,
        changed: &mut watch::Receiver<()>,
        mut next: impl FnMut() -> Result<OperationFuture, Failure> + Send,
    ) -> Result<Read, OutputReadError> {
        let bytes = self.transfer_reply(opening, generation, changed).await?;
        let metadata = match after {
            Some(after) => self.replay_outcome(&bytes, after)?,
            None => self.outcome(&bytes)?,
        };
        let mut content = Content::new(self, metadata, code, after)?;
        loop {
            let bytes = self
                .transfer_reply(next().map_err(failure_error)?, generation, changed)
                .await?;
            let page = match after {
                Some(after) => self.replay_outcome(&bytes, after)?,
                None => self.outcome(&bytes)?,
            };
            if content.append(self, page)? {
                break;
            }
            tokio::task::yield_now().await;
        }
        let read = content.finish()?;
        if !self.fence.is_current(generation) {
            return Err(OutputReadError::Retired);
        }
        Ok(read)
    }

    async fn transfer_reply(
        &self,
        operation: OperationFuture,
        generation: u64,
        changed: &mut watch::Receiver<()>,
    ) -> Result<ReceivedBytes, OutputReadError> {
        if !self.fence.is_current(generation) {
            return Err(OutputReadError::Retired);
        }
        self.fence.track(&operation);
        let bytes = tokio::select! {
            biased;
            _ = changed.changed() => return Err(OutputReadError::Retired),
            result = tokio::time::timeout(std::time::Duration::from_secs(30), operation) => {
                result.map_err(|_| OutputReadError::Failed(
                    anyhow::anyhow!("native output transfer page timed out"),
                ))?.map_err(failure_error)?
            }
        };
        if !self.fence.is_current(generation) {
            return Err(OutputReadError::Retired);
        }
        if bytes.len() > ext::MAX_READ_BYTES {
            return Err(invalid(
                "native output transfer page exceeds its negotiated byte bound",
            ));
        }
        Ok(bytes)
    }
}
