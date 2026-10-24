// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Direct Rust graph binding for the same implementation exported by the native
//! SQLite bootstrap plugin. No Source-plugin callback or event conversion.

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_bootstrap_sqlite::{
    NativeBootstrapProvider, SqliteBootstrapProvider as NativeSqliteBootstrapProvider,
};
use drasi_lib::computation::v1::{
    ComponentId, ComputationBootstrapProvider, ComputationBootstrapSnapshot, StreamId,
};

pub use drasi_computation_bootstrap_sqlite::SqliteBootstrapConfig;

pub struct SqliteBootstrapProvider(NativeSqliteBootstrapProvider);

impl SqliteBootstrapProvider {
    pub fn new(
        source: ComponentId,
        stream: StreamId,
        config: SqliteBootstrapConfig,
    ) -> Result<Self> {
        Ok(Self(NativeSqliteBootstrapProvider::new(
            source, stream, config,
        )?))
    }
}

#[async_trait]
impl ComputationBootstrapProvider for SqliteBootstrapProvider {
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.0.snapshot().await
    }

    async fn stop(&self) -> Result<()> {
        self.0.stop().await
    }
}
