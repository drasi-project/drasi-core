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

use async_trait::async_trait;

use crate::models::{ElementReference, ElementTimestamp};

use super::IndexError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushType {
    Always,
    IfNotExists,
    Overwrite,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct FutureElementRef {
    pub element_ref: ElementReference,
    pub original_time: ElementTimestamp,
    pub due_time: ElementTimestamp,
    pub group_signature: u64,
}

#[async_trait]
pub trait FutureQueue: Send + Sync {
    async fn push(
        &self,
        push_type: PushType,
        position_in_query: usize,
        group_signature: u64,
        element_ref: &ElementReference,
        original_time: ElementTimestamp,
        due_time: ElementTimestamp,
    ) -> Result<bool, IndexError>;

    async fn remove(
        &self,
        position_in_query: usize,
        group_signature: u64,
    ) -> Result<(), IndexError>;

    async fn pop(&self) -> Result<Option<FutureElementRef>, IndexError>;

    async fn peek_due_time(&self) -> Result<Option<ElementTimestamp>, IndexError>;

    async fn clear(&self) -> Result<(), IndexError>;
}

#[async_trait]
pub trait FutureQueueConsumer: Send + Sync {
    async fn on_due(
        &self,
        future_ref: &FutureElementRef,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    async fn on_error(
        &self,
        future_ref: &FutureElementRef,
        error: Box<dyn std::error::Error + Send + Sync>,
    );
    fn now(&self) -> u64;
}
