// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::SqliteHandle;
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::SourceSchemaReader;
use drasi_core_types::schema::SourceSchema;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::watch;

struct Generation {
    handle: SqliteHandle,
    active: AtomicBool,
}

pub struct SqliteSchemaReader {
    current: watch::Sender<Option<Arc<Generation>>>,
}

impl SqliteSchemaReader {
    pub(super) fn new() -> Self {
        Self {
            current: watch::channel(None).0,
        }
    }

    pub(super) fn activate(&self, handle: SqliteHandle) -> Result<()> {
        anyhow::ensure!(
            self.current.borrow().is_none(),
            "SQLite schema generation is still active"
        );
        self.current.send_replace(Some(Arc::new(Generation {
            handle,
            active: AtomicBool::new(true),
        })));
        Ok(())
    }

    pub(super) fn deactivate(&self) {
        self.current.send_modify(|current| {
            if let Some(generation) = current.take() {
                generation.active.store(false, Ordering::Release);
            }
        });
    }
}

#[async_trait]
impl SourceSchemaReader for SqliteSchemaReader {
    async fn describe_schema(&self) -> Result<Option<SourceSchema>> {
        let generation = self
            .current
            .borrow()
            .clone()
            .context("native SQLite source is not running")?;
        anyhow::ensure!(
            generation.active.load(Ordering::Acquire),
            "SQLite schema generation is retired"
        );
        let schema = generation.handle.describe_schema().await?;
        anyhow::ensure!(
            generation.active.load(Ordering::Acquire),
            "SQLite schema generation is retired"
        );
        Ok(Some(schema))
    }
}
