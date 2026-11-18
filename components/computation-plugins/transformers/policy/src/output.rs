// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Snapshot publication of the current decision elements.

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ElementImage, GraphChangeCodec, GraphProducerIdentity, OutputCursor, OutputReadError,
    OutputSnapshot, SnapshotProvider,
};
use drasi_core_types::models::{Element, ElementReference};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock, Weak},
};

pub(crate) type Rows = HashMap<ElementReference, Element>;

struct Published {
    cursor: OutputCursor,
    rows: Rows,
    running: bool,
    retired: bool,
}

pub(crate) struct Output {
    published: Arc<RwLock<Published>>,
    reader: Arc<Reader>,
}

const POISONED: &str = "policy snapshot state poisoned";

impl Output {
    pub(crate) fn new(identity: GraphProducerIdentity) -> Result<Self> {
        let published = Arc::new(RwLock::new(Published {
            cursor: OutputCursor::try_new(identity, 0)?,
            rows: Rows::new(),
            running: false,
            retired: false,
        }));
        let reader = Arc::new(Reader(Arc::downgrade(&published)));
        Ok(Self { published, reader })
    }

    pub(crate) fn reader(&self) -> Arc<dyn SnapshotProvider> {
        self.reader.clone()
    }

    pub(crate) fn set_running(&self, running: bool) -> Result<()> {
        let mut published = self
            .published
            .write()
            .map_err(|_| anyhow::anyhow!(POISONED))?;
        anyhow::ensure!(!published.retired, "policy state has been deprovisioned");
        published.running = running;
        Ok(())
    }

    /// Applies one emitted envelope's row changes at `sequence`.
    pub(crate) fn publish(
        &self,
        changes: impl IntoIterator<Item = (ElementReference, Option<Element>)>,
        sequence: u64,
    ) -> Result<()> {
        let mut published = self
            .published
            .write()
            .map_err(|_| anyhow::anyhow!(POISONED))?;
        anyhow::ensure!(
            published.running && !published.retired,
            "policy transformer is not accepting output"
        );
        for (reference, row) in changes {
            match row {
                Some(row) => published.rows.insert(reference, row),
                None => published.rows.remove(&reference),
            };
        }
        published.cursor = published.cursor.at_sequence(sequence);
        Ok(())
    }

    pub(crate) fn retire(&self) -> Result<()> {
        let mut published = self
            .published
            .write()
            .map_err(|_| anyhow::anyhow!(POISONED))?;
        anyhow::ensure!(
            !published.running,
            "stop the policy transformer before removing its state"
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
            let published = published
                .read()
                .map_err(|_| OutputReadError::Failed(anyhow::anyhow!(POISONED)))?;
            if published.retired {
                return Err(OutputReadError::Retired);
            }
            if !published.running {
                return Err(OutputReadError::NotReady(
                    "policy transformer is not running".into(),
                ));
            }
            (
                published.cursor.clone(),
                published.rows.values().cloned().collect::<Vec<_>>(),
            )
        };
        let records = rows
            .into_iter()
            .map(|row| {
                let record = GraphChangeCodec::element_record(&ElementImage::try_from(row)?)?;
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
