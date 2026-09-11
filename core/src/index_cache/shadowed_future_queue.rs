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

use crate::interface::SessionError;

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::{
    interface::{
        CacheGeneration, FutureElementRef, FutureQueue, IndexError, PushType, SessionControl,
    },
    models::{ElementReference, ElementTimestamp},
};

pub struct ShadowedFutureQueue {
    inner: Arc<dyn FutureQueue>,
    control: Option<Arc<dyn SessionControl>>,
    head_shadow: Mutex<(Option<CacheGeneration>, Option<Option<ElementTimestamp>>)>,
}

impl ShadowedFutureQueue {
    pub fn new(inner: Arc<dyn FutureQueue>) -> Self {
        Self {
            inner,
            control: None,
            head_shadow: Mutex::new((None, None)),
        }
    }

    pub fn new_with_session(inner: Arc<dyn FutureQueue>, control: Arc<dyn SessionControl>) -> Self {
        Self {
            inner,
            control: Some(control),
            head_shadow: Mutex::new((None, None)),
        }
    }

    fn check(&self, generation: CacheGeneration) -> Result<(), IndexError> {
        if self.generation()? != generation {
            return Err(IndexError::other(SessionError::StaleCacheGeneration));
        }
        Ok(())
    }

    fn generation(&self) -> Result<CacheGeneration, IndexError> {
        match &self.control {
            Some(control) => crate::interface::session_tracker(control)?.cache_generation(),
            None => Ok(CacheGeneration::untracked()),
        }
    }
}

#[async_trait]
impl FutureQueue for ShadowedFutureQueue {
    async fn push(
        &self,
        push_type: PushType,
        position_in_query: usize,
        group_signature: u64,
        element_ref: &ElementReference,
        original_time: ElementTimestamp,
        due_time: ElementTimestamp,
    ) -> Result<bool, IndexError> {
        let mut shadow = self.head_shadow.lock().await;
        let generation = self.generation()?;
        *shadow = (None, None);
        let result = self
            .inner
            .push(
                push_type,
                position_in_query,
                group_signature,
                element_ref,
                original_time,
                due_time,
            )
            .await?;
        self.check(generation)?;
        Ok(result)
    }

    async fn remove(
        &self,
        position_in_query: usize,
        group_signature: u64,
    ) -> Result<(), IndexError> {
        let mut shadow = self.head_shadow.lock().await;
        let generation = self.generation()?;
        *shadow = (None, None);
        self.inner
            .remove(position_in_query, group_signature)
            .await?;
        self.check(generation)
    }

    async fn pop(&self) -> Result<Option<FutureElementRef>, IndexError> {
        let mut shadow = self.head_shadow.lock().await;
        let generation = self.generation()?;
        *shadow = (None, None);
        let result = self.inner.pop().await?;
        self.check(generation)?;
        Ok(result)
    }

    async fn peek_due_time(&self) -> Result<Option<ElementTimestamp>, IndexError> {
        let mut shadow = self.head_shadow.lock().await;
        let generation = self.generation()?;
        if self.control.is_some() && shadow.0 == Some(generation) {
            if let Some(value) = shadow.1 {
                return Ok(value);
            }
        }
        let value = self.inner.peek_due_time().await?;
        self.check(generation)?;
        *shadow = (Some(generation), Some(value));
        Ok(value)
    }

    async fn clear(&self) -> Result<(), IndexError> {
        let mut shadow = self.head_shadow.lock().await;
        let generation = self.generation()?;
        *shadow = (None, None);
        self.inner.clear().await?;
        self.check(generation)
    }
}
