// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{ChangeEnvelope, StreamId};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core_types::interface::StorageDurability;
use futures_core::Stream;
use std::pin::Pin;
pub const MAX_BOOTSTRAP_STATE_BYTES: usize = 65536;

/// Bounded connector handover metadata in the query's own transaction domain.
/// Writes finish before external initialization effects may begin. State survives
/// an explicit query reset, so a connector can identify its unfinished resources.
/// Query deprovisioning removes it. The provider must not retain this borrowed service.
#[async_trait]
pub trait BootstrapState: Send + Sync {
    fn durability(&self) -> StorageDurability;
    async fn read(&self) -> anyhow::Result<Option<Bytes>>;
    async fn write(&self, state: Bytes) -> anyhow::Result<()>;
}

pub struct BootstrapWatermark {
    pub stream: StreamId,
    pub source_id: Option<String>,
    pub sequence: u64,
    pub position: Option<Bytes>,
    /// The output epoch observed with a generic producer snapshot. Source-driver
    /// positions use `source_id`/`position` instead; the two scopes cannot mix.
    pub producer: Option<crate::GraphProducerIdentity>,
}

impl BootstrapWatermark {
    pub fn from_output(cursor: &crate::OutputCursor) -> Self {
        Self {
            stream: cursor.producer().stream().clone(),
            source_id: None,
            sequence: cursor.sequence(),
            position: None,
            producer: Some(cursor.producer().clone()),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        validate_bootstrap_watermark(
            &self.stream,
            self.source_id.as_deref(),
            self.position.as_deref(),
            self.producer.as_ref(),
        )
    }
}

pub fn validate_bootstrap_watermark(
    stream: &StreamId,
    source_id: Option<&str>,
    position: Option<&[u8]>,
    producer: Option<&crate::GraphProducerIdentity>,
) -> anyhow::Result<()> {
    if let Some(producer) = producer {
        producer.validate()?;
        anyhow::ensure!(
            producer.stream() == stream && source_id.is_none() && position.is_none(),
            "output snapshot watermark mixes producer and source-driver positions"
        );
    }
    anyhow::ensure!(
        position.map_or(true, |position| position.len()
            <= crate::MAX_SOURCE_POSITION_BYTES),
        "bootstrap source position exceeds the supported checkpoint limit"
    );
    Ok(())
}

pub struct ComputationBootstrapSnapshot {
    pub changes: Pin<Box<dyn Stream<Item = anyhow::Result<ChangeEnvelope>> + Send>>,
    pub watermarks: Vec<BootstrapWatermark>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapPreparation {
    Ready,
    RefreshVolatile,
    ResetRequired,
}
