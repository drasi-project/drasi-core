// Copyright 2025 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Result;
use async_trait::async_trait;
use drasi_http_common::bootstrap::reader::{HttpSnapshotReader, SnapshotRequest};
use drasi_lib::{
    bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult},
    channels::{BootstrapEvent, BootstrapEventSender},
    config::SourceSubscriptionSettings,
};

use crate::config::HttpBootstrapConfig;

/// Legacy lifecycle binding for the shared REST snapshot reader.
pub struct HttpBootstrapProvider {
    reader: HttpSnapshotReader,
}

impl HttpBootstrapProvider {
    pub fn new(config: HttpBootstrapConfig) -> Result<Self> {
        Ok(Self {
            reader: HttpSnapshotReader::new(config)?,
        })
    }
}

#[async_trait]
impl BootstrapProvider for HttpBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        let event_count = self
            .reader
            .read(
                SnapshotRequest {
                    source_id: &context.source_id,
                    node_labels: &request.node_labels,
                    relation_labels: &request.relation_labels,
                },
                |change| {
                    let event = BootstrapEvent {
                        source_id: context.source_id.clone(),
                        change,
                        timestamp: chrono::Utc::now(),
                        sequence: context.next_sequence(),
                    };
                    let sender = &event_tx;
                    async move {
                        sender.send(event).await?;
                        Ok(())
                    }
                },
            )
            .await?;
        Ok(BootstrapResult {
            event_count: usize::try_from(event_count)?,
            source_position: None,
        })
    }
}
