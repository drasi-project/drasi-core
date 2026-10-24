// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

#[async_trait]
impl NativeSource for MsSqlSource {
    fn source_schema_reader(&self) -> Option<Arc<dyn SourceSchemaReader>> {
        Some(self.schema_reader.clone())
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        ensure!(
            self.cleanup && self.generation.load(AtomicOrdering::Acquire) % 2 == 1,
            "SQL Server source is not running"
        );
        if !self.initialized {
            let snapshot = self.progress.wait_ready().await?;
            self.cursor = self.observe(&snapshot)?.or_else(|| self.origin.clone());
            self.initialized = true;
        }
        loop {
            self.observe(&self.progress.snapshot()?)?;
            if let Some(change) = self.queue.pop_front() {
                self.sequence = self
                    .sequence
                    .checked_add(1)
                    .context("SQL Server sequence exhausted")?;
                let event = SourceEventWrapper::new(
                    self.descriptor.id().to_string(),
                    SourceEvent::Change(change.change),
                    chrono::Utc::now(),
                    self.sequence,
                )
                .with_source_position(change.position.encode()?.into());
                let mut envelope = GraphChangeCodec::encode_source_event(
                    Arc::new(event),
                    self.descriptor.id(),
                    self.stream.clone(),
                    self.sequence,
                    None,
                )?;
                if let Some(before) = change.before {
                    let before = GraphChangeCodec::encode_change(
                        drasi_core_types::models::SourceChange::Insert { element: before },
                        self.stream.clone(),
                        self.sequence,
                        None,
                    )?;
                    use drasi_computation_contracts::{ChangeOperation, ChangeSet};
                    let ChangeOperation::Added { after: before, .. } =
                        &before.changes().operations()[0]
                    else {
                        anyhow::bail!("invalid SQL Server before-image");
                    };
                    let mut operation = envelope.changes().operations()[0].clone();
                    match &mut operation {
                        ChangeOperation::Updated { before: image, .. }
                        | ChangeOperation::Deleted { before: image, .. } => {
                            *image = Some(before.clone())
                        }
                        _ => anyhow::bail!("unexpected SQL Server before-image operation"),
                    }
                    let changes = ChangeSet::try_new(
                        envelope.changes().id().clone(),
                        envelope.changes().schema().clone(),
                        vec![operation],
                    )?;
                    envelope = envelope.derive(
                        envelope.id().clone(),
                        changes,
                        envelope.system().as_ref().clone(),
                    );
                }
                return Ok(Some(OutputEnvelope {
                    port: PortId::try_new("out")?,
                    envelope,
                }));
            }
            if !self.work.pending() {
                let config = self.config.clone();
                let source = self.descriptor.id().to_string();
                let cursor = self.cursor.clone();
                self.work
                    .start(async move { native::poll(&config, &source, cursor.as_ref()).await })?;
            }
            let fresh_current = self.cursor.is_none()
                && self.config.connection.start_position
                    == drasi_mssql_common::StartPosition::Current;
            let batch = self.work.wait(self.config.timeout()).await?;
            if self.origin.is_none() && fresh_current {
                self.origin = Some(batch.boundary.clone());
            }
            self.cursor = Some(batch.boundary);
            self.queue = batch.changes.into();
            if self.queue.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(
                    self.config.connection.poll_interval_ms,
                ))
                .await;
            }
        }
    }
}
