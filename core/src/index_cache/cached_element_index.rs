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

use std::{collections::HashMap, sync::Arc};

use async_stream::stream;
use async_trait::async_trait;
use tokio::sync::RwLock;
use tokio_stream::StreamExt;

use super::lru::SafeLru;

use crate::{
    interface::{ElementIndex, ElementStream, IndexError},
    models::{Element, ElementReference, QueryJoin},
    path_solver::match_path::MatchPath,
};

pub struct CachedElementIndex {
    element_index: Arc<dyn ElementIndex>,
    element_cache: Arc<RwLock<SafeLru<ElementReference, Arc<Element>>>>,
    slot_cache: Arc<RwLock<SafeLru<ElementReference, HashMap<usize, bool>>>>,
    inbound_cache: Arc<RwLock<SafeLru<(ElementReference, usize), Vec<ElementReference>>>>,
    outbound_cache: Arc<RwLock<SafeLru<(ElementReference, usize), Vec<ElementReference>>>>,
}

impl CachedElementIndex {
    pub fn new(element_index: Arc<dyn ElementIndex>, cache_size: usize) -> Self {
        log::info!("using cached element index with size {cache_size}");

        Self {
            element_index,
            element_cache: Arc::new(RwLock::new(SafeLru::new(cache_size))),
            slot_cache: Arc::new(RwLock::new(SafeLru::new(cache_size))),
            inbound_cache: Arc::new(RwLock::new(SafeLru::new(cache_size))),
            outbound_cache: Arc::new(RwLock::new(SafeLru::new(cache_size))),
        }
    }
}

#[async_trait]
impl ElementIndex for CachedElementIndex {
    async fn get_element(
        &self,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        let element_index = self.element_index.clone();
        let cache = self.element_cache.clone();

        get_element_internal(element_index, cache, element_ref).await
    }

    async fn set_element(
        &self,
        element: &Element,
        slot_affinity: &Vec<usize>,
    ) -> Result<(), IndexError> {
        self.element_index
            .set_element(element, slot_affinity)
            .await?;

        let mut element_cache = self.element_cache.write().await;
        element_cache.put(element.get_reference().clone(), Arc::new(element.clone()));

        let mut slot_cache = self.slot_cache.write().await;
        slot_cache.remove(element.get_reference());

        let mut inbound_cache = self.inbound_cache.write().await;
        inbound_cache.purge();

        let mut outbound_cache = self.outbound_cache.write().await;
        outbound_cache.purge();

        Ok(())
    }

    async fn delete_element(&self, element_ref: &ElementReference) -> Result<(), IndexError> {
        self.element_index.delete_element(element_ref).await?;

        let mut element_cache = self.element_cache.write().await;
        element_cache.remove(element_ref);

        let mut slot_cache = self.slot_cache.write().await;
        slot_cache.remove(element_ref);

        let mut inbound_cache = self.inbound_cache.write().await;
        inbound_cache.purge();

        let mut outbound_cache = self.outbound_cache.write().await;
        outbound_cache.purge();

        Ok(())
    }

    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        // `caches` LRU nodes are raw pointers. Never hold a reference into the
        // cache (or its write lock) across an `.await` — eviction can free the
        // node and SIGSEGV.
        let cached = {
            let mut slot_cache = self.slot_cache.write().await;
            slot_cache
                .get(element_ref)
                .and_then(|slot_elements| slot_elements.get(&slot).copied())
        };

        match cached {
            Some(true) => self.get_element(element_ref).await,
            Some(false) => Ok(None),
            None => {
                let result = self
                    .element_index
                    .get_slot_element_by_ref(slot, element_ref)
                    .await?;

                let mut slot_cache = self.slot_cache.write().await;
                match slot_cache.get_mut(element_ref) {
                    Some(slot_elements) => {
                        slot_elements.insert(slot, result.is_some());
                    }
                    None => {
                        let mut slot_set = HashMap::new();
                        slot_set.insert(slot, result.is_some());
                        slot_cache.put(element_ref.clone(), slot_set);
                    }
                }

                Ok(result)
            }
        }
    }

    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        inbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        let mut inbound_cache = self.inbound_cache.write().await;
        let key = (inbound_ref.clone(), slot);
        match inbound_cache.get(&key) {
            Some(elements) => {
                let elements = elements.clone();
                drop(inbound_cache);
                let element_cache = self.element_cache.clone();
                let element_index = self.element_index.clone();
                let stream = stream! {
                    for element_ref in elements {
                        match get_element_internal(element_index.clone(), element_cache.clone(), &element_ref).await? {
                            Some(element) => yield Ok(element),
                            None => continue,
                        }
                    }
                };
                Ok(Box::pin(stream))
            }
            None => {
                drop(inbound_cache);
                let cache_source = self.inbound_cache.clone();
                let element_index = self.element_index.clone();
                let inbound_ref = inbound_ref.clone();
                let stream = stream! {
                    let mut element_stream = element_index.get_slot_elements_by_inbound(slot, &inbound_ref).await?;
                    let mut elements = Vec::new();
                    while let Some(element) = element_stream.next().await {
                        match element {
                            Ok(element) => {
                                elements.push(element.get_reference().clone());
                                yield Ok(element);
                            },
                            Err(err) => {
                                yield Err(err);
                            }
                        };
                    }

                    let mut inbound_cache = cache_source.write().await;
                    inbound_cache.put((inbound_ref, slot), elements);
                    drop(inbound_cache);
                };
                Ok(Box::pin(stream))
            }
        }
    }

    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        outbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        let mut outbound_cache = self.outbound_cache.write().await;
        let key = (outbound_ref.clone(), slot);
        match outbound_cache.get(&key) {
            Some(elements) => {
                let elements = elements.clone();
                drop(outbound_cache);
                let element_cache = self.element_cache.clone();
                let element_index = self.element_index.clone();
                let stream = stream! {
                    for element_ref in elements {
                        match get_element_internal(element_index.clone(), element_cache.clone(), &element_ref).await? {
                            Some(element) => yield Ok(element),
                            None => continue,
                        }
                    }
                };
                Ok(Box::pin(stream))
            }
            None => {
                drop(outbound_cache);
                let cache_source = self.outbound_cache.clone();
                let element_index = self.element_index.clone();
                let outbound_ref = outbound_ref.clone();
                let stream = stream! {
                    let mut element_stream = element_index.get_slot_elements_by_outbound(slot, &outbound_ref).await?;
                    let mut elements = Vec::new();
                    while let Some(element) = element_stream.next().await {
                        match element {
                            Ok(element) => {
                                elements.push(element.get_reference().clone());
                                yield Ok(element);
                            },
                            Err(err) => {
                                yield Err(err);
                            }
                        };
                    }

                    let mut outbound_cache = cache_source.write().await;
                    outbound_cache.put((outbound_ref, slot), elements);
                    drop(outbound_cache);
                };
                Ok(Box::pin(stream))
            }
        }
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.element_index.clear().await?;

        let mut element_cache = self.element_cache.write().await;
        element_cache.purge();

        let mut slot_cache = self.slot_cache.write().await;
        slot_cache.purge();

        let mut inbound_cache = self.inbound_cache.write().await;
        inbound_cache.purge();

        let mut outbound_cache = self.outbound_cache.write().await;
        outbound_cache.purge();

        Ok(())
    }

    async fn set_joins(&self, match_path: &MatchPath, joins: &Vec<Arc<QueryJoin>>) {
        self.element_index.set_joins(match_path, joins).await;
    }
}

async fn get_element_internal(
    element_index: Arc<dyn ElementIndex>,
    cache: Arc<RwLock<SafeLru<ElementReference, Arc<Element>>>>,
    element_ref: &ElementReference,
) -> Result<Option<Arc<Element>>, IndexError> {
    let mut element_cache = cache.write().await;
    let element = element_cache.get(element_ref);
    match element {
        Some(element) => Ok(Some(element.clone())),
        None => {
            drop(element_cache);
            let element = element_index.get_element(element_ref).await?;
            match element {
                Some(element) => {
                    let mut element_cache = cache.write().await;
                    element_cache.put(element_ref.clone(), element.clone());
                    Ok(Some(element))
                }
                None => Ok(None),
            }
        }
    }
}
