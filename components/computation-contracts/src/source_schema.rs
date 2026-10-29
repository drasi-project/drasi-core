// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Read-only source data discovery, independent of mutable envelope processing.

use async_trait::async_trait;
use drasi_core_types::schema::SourceSchema;

/// Reads the actual source's data schema, not its factory configuration schema.
///
/// Implementations must bound their work and reject reads against retired
/// source state. `None` means no schema is available; failures remain errors.
#[async_trait]
pub trait SourceSchemaReader: Send + Sync {
    async fn describe_schema(&self) -> anyhow::Result<Option<SourceSchema>>;
}
