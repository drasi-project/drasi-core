// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::{
    GraphChangeCodec, GraphProducerIdentity, OutputCursor, OutputReadError, OutputSnapshot,
    SnapshotProvider,
};
use std::sync::{Arc, RwLock, Weak};

use crate::state::Rows;

struct Published {
    cursor: OutputCursor,
    rows: Rows,
    running: bool,
    retired: bool,
}

pub(super) struct Output {
    published: Arc<RwLock<Published>>,
    reader: Arc<Reader>,
}

impl Output {
    pub(super) fn new(identity: GraphProducerIdentity) -> Result<Self> {
        let published = Arc::new(RwLock::new(Published {
            cursor: OutputCursor::try_new(identity, 0)?,
            rows: Rows::new(),
            running: false,
            retired: false,
        }));
        let reader = Arc::new(Reader(Arc::downgrade(&published)));
        Ok(Self { published, reader })
    }

    pub(super) fn reader(&self) -> Arc<dyn SnapshotProvider> {
        self.reader.clone()
    }

    pub(super) fn set_running(&self, running: bool) -> Result<()> {
        let mut published = self
            .published
            .write()
            .map_err(|_| anyhow::anyhow!("view snapshot state poisoned"))?;
        anyhow::ensure!(!published.retired, "view state has been deprovisioned");
        published.running = running;
        Ok(())
    }

    pub(super) fn publish(&self, rows: Rows, sequence: u64) -> Result<()> {
        let mut published = self
            .published
            .write()
            .map_err(|_| anyhow::anyhow!("view snapshot state poisoned"))?;
        anyhow::ensure!(
            published.running && !published.retired,
            "view is not accepting output"
        );
        published.rows = rows;
        published.cursor = published.cursor.at_sequence(sequence);
        Ok(())
    }

    pub(super) fn retire(&self) -> Result<()> {
        let mut published = self
            .published
            .write()
            .map_err(|_| anyhow::anyhow!("view snapshot state poisoned"))?;
        anyhow::ensure!(
            !published.running,
            "stop the view before removing its state"
        );
        published.retired = true;
        published.rows.clear();
        Ok(())
    }
}

struct Reader(Weak<RwLock<Published>>);

#[async_trait]
impl SnapshotProvider for Reader {
    async fn snapshot(&self) -> Result<OutputSnapshot, OutputReadError> {
        let published = self.0.upgrade().ok_or(OutputReadError::Retired)?;
        let (cursor, rows) = {
            let published = published.read().map_err(|_| {
                OutputReadError::Failed(anyhow::anyhow!("view snapshot state poisoned"))
            })?;
            if published.retired {
                return Err(OutputReadError::Retired);
            }
            if !published.running {
                return Err(OutputReadError::NotReady("view is not running".into()));
            }
            (published.cursor.clone(), published.rows.clone())
        };
        let records = rows
            .values()
            .map(|row| {
                let record = GraphChangeCodec::element_record(&row.image)?;
                Ok((record.identity().clone(), record))
            })
            .collect::<Result<_>>()
            .map_err(OutputReadError::Failed)?;
        let dataset = cursor.producer().component_id().to_string();
        OutputSnapshot::try_new(
            cursor,
            dataset,
            GraphChangeCodec::schema().descriptor().clone(),
            records,
        )
    }
}
