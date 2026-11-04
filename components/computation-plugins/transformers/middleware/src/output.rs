// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ElementImage, GraphChangeCodec, GraphProducerIdentity, OutputCursor, OutputReadError,
    OutputSnapshot, SnapshotProvider,
};
use drasi_core_types::models::{Element, ElementReference, SourceChange};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, RwLock,
};

struct Published {
    cursor: OutputCursor,
    rows: im::HashMap<ElementReference, Element>,
    ready: bool,
}

pub(super) struct Output {
    enabled: bool,
    failed: Arc<AtomicBool>,
    published: RwLock<Published>,
}

impl Output {
    pub(super) fn new(
        enabled: bool,
        failed: Arc<AtomicBool>,
        identity: GraphProducerIdentity,
    ) -> Result<Self> {
        Ok(Self {
            enabled,
            failed,
            published: RwLock::new(Published {
                cursor: OutputCursor::try_new(identity, 0)?,
                rows: im::HashMap::new(),
                ready: false,
            }),
        })
    }

    pub(super) fn reset(&self, identity: GraphProducerIdentity) -> Result<()> {
        *self
            .published
            .write()
            .map_err(|_| anyhow::anyhow!("middleware snapshot state poisoned"))? = Published {
            cursor: OutputCursor::try_new(identity, 0)?,
            rows: im::HashMap::new(),
            ready: false,
        };
        Ok(())
    }

    pub(super) fn ready(&self) -> Result<bool> {
        Ok(!self.enabled
            || self
                .published
                .read()
                .map_err(|_| anyhow::anyhow!("middleware snapshot state poisoned"))?
                .ready)
    }

    pub(super) fn activate(&self) -> Result<()> {
        self.published
            .write()
            .map_err(|_| anyhow::anyhow!("middleware snapshot state poisoned"))?
            .ready = true;
        Ok(())
    }

    pub(super) fn stop(&self) -> Result<()> {
        let mut published = self
            .published
            .write()
            .map_err(|_| anyhow::anyhow!("middleware snapshot state poisoned"))?;
        published.ready = false;
        published.rows.clear();
        Ok(())
    }

    pub(super) fn apply(&self, changes: &[SourceChange], sequence: u64) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut published = self
            .published
            .write()
            .map_err(|_| anyhow::anyhow!("middleware snapshot state poisoned"))?;
        let mut rows = published.rows.clone();
        for change in changes {
            change.validate()?;
            match change {
                SourceChange::Insert { element } | SourceChange::Replace { element } => {
                    rows.insert(element.get_reference().clone(), element.clone());
                }
                SourceChange::Update {
                    element,
                    removed_properties,
                } => {
                    let mut element = element.clone();
                    if let Some(previous) = rows.get(element.get_reference()) {
                        anyhow::ensure!(
                            std::mem::discriminant(&element) == std::mem::discriminant(previous),
                            "middleware snapshot update changed an element's kind"
                        );
                        element.merge_missing_properties_except(previous, removed_properties);
                    }
                    rows.insert(element.get_reference().clone(), element);
                }
                SourceChange::Delete { metadata } => {
                    rows.remove(&metadata.reference);
                }
                SourceChange::Future { .. } => {}
            }
        }
        published.rows = rows;
        published.cursor = published.cursor.at_sequence(sequence);
        Ok(())
    }
}

#[async_trait]
impl SnapshotProvider for Output {
    async fn snapshot(&self) -> Result<OutputSnapshot, OutputReadError> {
        let (cursor, rows) = {
            let published = self.published.read().map_err(|_| {
                OutputReadError::Failed(anyhow::anyhow!("middleware snapshot state poisoned"))
            })?;
            if !self.enabled {
                return Err(OutputReadError::Failed(anyhow::anyhow!(
                    "middleware snapshots require bootstrap: true"
                )));
            }
            if !published.ready || self.failed.load(Ordering::Acquire) {
                return Err(OutputReadError::NotReady(
                    "middleware initialization is not complete".into(),
                ));
            }
            (published.cursor.clone(), published.rows.clone())
        };
        let records = rows
            .values()
            .map(|element| {
                let record =
                    GraphChangeCodec::element_record(&ElementImage::try_from(element.clone())?)?;
                Ok((record.identity().clone(), record))
            })
            .collect::<Result<im::HashMap<_, _>>>()
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
