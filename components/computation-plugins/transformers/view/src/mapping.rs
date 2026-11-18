// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_middleware_common::Pipeline;
use drasi_core_types::models::Element;

/// Maps one complete input image to its projected images. The shared
/// middleware pipeline is the production mapper; tests substitute others.
#[async_trait]
pub(super) trait Mapper: Send + Sync {
    async fn map(&self, element: Element) -> Result<Vec<Element>>;
}

#[async_trait]
impl Mapper for Pipeline {
    async fn map(&self, element: Element) -> Result<Vec<Element>> {
        Pipeline::map(self, element).await
    }
}
