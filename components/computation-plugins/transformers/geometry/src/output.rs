// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The published snapshot of the current relation and error records.

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ElementImage, GraphChangeCodec, GraphProducerIdentity, OutputCursor, OutputReadError,
    OutputSnapshot, SnapshotProvider,
};
use std::sync::{Arc, RwLock, Weak};

use crate::state::Records;

struct Published {
    cursor: OutputCursor,
    records: Arc<Records>,
    running: bool,
    retired: bool,
}

pub(crate) struct Output {
    published: Arc<RwLock<Published>>,
    reader: Arc<Reader>,
}

impl Output {
    pub(crate) fn new(identity: GraphProducerIdentity, namespace: Arc<str>) -> Result<Self> {
        let published = Arc::new(RwLock::new(Published {
            cursor: OutputCursor::try_new(identity, 0)?,
            records: Arc::new(Records::new()),
            running: false,
            retired: false,
        }));
        let reader = Arc::new(Reader {
            published: Arc::downgrade(&published),
            namespace,
        });
        Ok(Self { published, reader })
    }

    pub(crate) fn reader(&self) -> Arc<dyn SnapshotProvider> {
        self.reader.clone()
    }

    fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, Published>> {
        self.published
            .write()
            .map_err(|_| anyhow::anyhow!("geometry snapshot state poisoned"))
    }

    pub(crate) fn set_running(&self, running: bool) -> Result<()> {
        let mut published = self.write()?;
        anyhow::ensure!(!published.retired, "geometry state has been deprovisioned");
        published.running = running;
        Ok(())
    }

    pub(crate) fn publish(&self, records: Arc<Records>, sequence: u64) -> Result<()> {
        let mut published = self.write()?;
        anyhow::ensure!(
            published.running && !published.retired,
            "geometry transformer is not accepting output"
        );
        published.records = records;
        published.cursor = published.cursor.at_sequence(sequence);
        Ok(())
    }

    pub(crate) fn retire(&self) -> Result<()> {
        let mut published = self.write()?;
        anyhow::ensure!(
            !published.running,
            "stop the geometry transformer before removing its state"
        );
        published.retired = true;
        published.records = Arc::new(Records::new());
        Ok(())
    }
}

struct Reader {
    published: Weak<RwLock<Published>>,
    namespace: Arc<str>,
}

#[async_trait]
impl SnapshotProvider for Reader {
    async fn snapshot(&self) -> Result<OutputSnapshot, OutputReadError> {
        let published = self.published.upgrade().ok_or(OutputReadError::Retired)?;
        let (cursor, records) = {
            let published = published.read().map_err(|_| {
                OutputReadError::Failed(anyhow::anyhow!("geometry snapshot state poisoned"))
            })?;
            if published.retired {
                return Err(OutputReadError::Retired);
            }
            if !published.running {
                return Err(OutputReadError::NotReady(
                    "geometry transformer is not running".into(),
                ));
            }
            (published.cursor.clone(), published.records.clone())
        };
        let records = records
            .values()
            .map(|record| {
                let image = ElementImage::try_from(record.element(&self.namespace))?;
                let record = GraphChangeCodec::element_record(&image)?;
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
