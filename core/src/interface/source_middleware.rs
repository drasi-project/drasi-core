// Copyright 2024 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::models::{SourceChange, SourceMiddlewareConfig};

#[derive(Error, Debug)]
pub enum MiddlewareError {
    #[error("Error processing source change: {0}")]
    SourceChangeError(String),

    #[error("Unknown middleware {0}")]
    UnknownKind(String),
}

#[derive(Error, Debug)]
pub enum MiddlewareSetupError {
    #[error("Invalid configuration: {0}")]
    InvalidConfiguration(String),
    #[error("No registry found for middleware")]
    NoRegistry,
}

#[async_trait]
pub trait SourceMiddleware: Send + Sync {
    async fn process(
        &self,
        source_change: SourceChange,
    ) -> Result<Vec<SourceChange>, MiddlewareError>;
}

pub trait SourceMiddlewareFactory: Send + Sync {
    fn name(&self) -> String;
    fn create(
        &self,
        config: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError>;
    //todo: inject dependencies such as element index, expression evaluator, etc.
}
