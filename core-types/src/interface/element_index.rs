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

use std::{collections::HashMap, pin::Pin, sync::Arc};

use async_trait::async_trait;
use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::models::{Element, ElementReference, ElementTimestamp, QueryJoin, TimestampRange};

use super::IndexError;

pub type ElementResult = Result<Arc<Element>, IndexError>;
pub type ElementStream = Pin<Box<dyn Stream<Item = ElementResult> + Send>>;

/// A synthetic join and the nonempty match slots in which its relationship is used.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexJoin {
    pub join: Arc<QueryJoin>,
    pub slots: Vec<usize>,
}

pub type IndexJoinsByLabel = HashMap<String, Vec<(Arc<QueryJoin>, Vec<usize>)>>;

/// Group configured joins by their source element labels, preserving key order
/// and repeated keys within each label.
pub fn index_joins_by_label(joins: &[IndexJoin]) -> Result<IndexJoinsByLabel, IndexError> {
    let mut result = IndexJoinsByLabel::new();
    for spec in joins {
        if spec.slots.is_empty() {
            return Err(IndexError::other(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "an indexed join must have at least one match slot",
            )));
        }
        for key in &spec.join.keys {
            result
                .entry(key.label.clone())
                .or_default()
                .push((spec.join.clone(), spec.slots.clone()));
        }
    }
    Ok(result)
}

#[async_trait]
pub trait ElementIndex: Send + Sync {
    async fn get_element(
        &self,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError>;
    async fn set_element(
        &self,
        element: &Element,
        slot_affinity: &Vec<usize>,
    ) -> Result<(), IndexError>;
    async fn delete_element(&self, element_ref: &ElementReference) -> Result<(), IndexError>;
    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError>;
    /// Enumerate the current elements matching a slot, including this session's writes.
    /// Disconnected MATCH components require this capability.
    async fn get_slot_elements(&self, _slot: usize) -> Result<ElementStream, IndexError> {
        Err(IndexError::NotSupported)
    }
    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        inbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError>;
    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        outbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError>;
    async fn clear(&self) -> Result<(), IndexError>;

    /// Configure synthetic joins before evaluation; failures must stop construction.
    async fn set_joins(&self, joins: &[IndexJoin]) -> Result<(), IndexError>;
}

#[async_trait]
pub trait ElementArchiveIndex: Send + Sync {
    async fn get_element_as_at(
        &self,
        element_ref: &ElementReference,
        time: ElementTimestamp,
    ) -> Result<Option<Arc<Element>>, IndexError>;
    async fn get_element_versions(
        &self,
        element_ref: &ElementReference,
        range: TimestampRange<ElementTimestamp>,
    ) -> Result<ElementStream, IndexError>;
    async fn clear(&self) -> Result<(), IndexError>;
}
