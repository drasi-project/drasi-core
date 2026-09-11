// Copyright 2026 The Drasi Authors.
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

use crate::{
    interface::{ElementIndex, ElementStream, IndexError, SessionTracker},
    models::{Element, ElementReference, QueryJoin},
    path_solver::match_path::MatchPath,
};

pub(crate) struct TrackedElementIndex {
    pub inner: Arc<dyn ElementIndex>,
    pub tracker: SessionTracker,
}

#[async_trait]
impl ElementIndex for TrackedElementIndex {
    async fn get_element(
        &self,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        self.inner.get_element(element_ref).await
    }

    async fn set_element(
        &self,
        element: &Element,
        slot_affinity: &Vec<usize>,
    ) -> Result<(), IndexError> {
        self.tracker.mark_dirty()?;
        self.inner.set_element(element, slot_affinity).await
    }

    async fn delete_element(&self, element_ref: &ElementReference) -> Result<(), IndexError> {
        self.tracker.mark_dirty()?;
        self.inner.delete_element(element_ref).await
    }

    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        self.inner.get_slot_element_by_ref(slot, element_ref).await
    }

    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        inbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        self.inner.get_slot_elements_by_inbound(slot, inbound_ref).await
    }

    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        outbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        self.inner.get_slot_elements_by_outbound(slot, outbound_ref).await
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.tracker.mark_dirty()?;
        self.inner.clear().await
    }

    async fn set_joins(&self, match_path: &MatchPath, joins: &Vec<Arc<QueryJoin>>) {
        if let Err(error) = self.tracker.mark_dirty() {
            log::error!("Cannot change middleware index joins outside an active root: {error}");
            return;
        }
        self.inner.set_joins(match_path, joins).await;
    }
}
