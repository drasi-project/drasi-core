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

use async_stream::try_stream;
use async_trait::async_trait;
use caches::lru::CacheError;
use tokio_stream::StreamExt;

use super::generation_cache::GenerationCache;
use crate::{
    interface::{CacheGeneration, ElementIndex, ElementStream, IndexError, SessionControl},
    models::{Element, ElementReference, QueryJoin},
    path_solver::match_path::MatchPath,
};

type AdjacencyCache = GenerationCache<(ElementReference, usize), Vec<ElementReference>>;

pub struct CachedElementIndex {
    element_index: Arc<dyn ElementIndex>,
    element_cache: Arc<GenerationCache<ElementReference, Arc<Element>>>,
    slot_cache: GenerationCache<(ElementReference, usize), bool>,
    inbound_cache: Arc<AdjacencyCache>,
    outbound_cache: Arc<AdjacencyCache>,
}

impl CachedElementIndex {
    pub fn new(
        element_index: Arc<dyn ElementIndex>,
        cache_size: usize,
    ) -> Result<Self, CacheError> {
        Self::with_control(element_index, cache_size, None)
    }

    pub fn new_with_session(
        element_index: Arc<dyn ElementIndex>,
        cache_size: usize,
        session_control: Arc<dyn SessionControl>,
    ) -> Result<Self, CacheError> {
        Self::with_control(element_index, cache_size, Some(session_control))
    }

    fn with_control(
        element_index: Arc<dyn ElementIndex>,
        cache_size: usize,
        session_control: Option<Arc<dyn SessionControl>>,
    ) -> Result<Self, CacheError> {
        Ok(Self {
            element_index,
            element_cache: Arc::new(GenerationCache::new(cache_size, session_control.clone())?),
            slot_cache: GenerationCache::new(cache_size, session_control.clone())?,
            inbound_cache: Arc::new(GenerationCache::new(cache_size, session_control.clone())?),
            outbound_cache: Arc::new(GenerationCache::new(cache_size, session_control)?),
        })
    }

    async fn invalidate(&self, generation: CacheGeneration) -> Result<(), IndexError> {
        self.element_cache.clear(generation).await?;
        self.slot_cache.clear(generation).await?;
        self.inbound_cache.clear(generation).await?;
        self.outbound_cache.clear(generation).await
    }

    async fn adjacent(
        &self,
        slot: usize,
        reference: &ElementReference,
        inbound: bool,
    ) -> Result<ElementStream, IndexError> {
        let cache = if inbound {
            &self.inbound_cache
        } else {
            &self.outbound_cache
        }
        .clone();
        let key = (reference.clone(), slot);
        let (generation, cached) = cache.get(&key).await?;
        let index = self.element_index.clone();
        let elements = self.element_cache.clone();
        Ok(Box::pin(try_stream! {
            cache.check(generation)?;
            if let Some(references) = cached {
                for reference in references {
                    let element = get_element_internal(&index, &elements, &reference).await?;
                    cache.check(generation)?;
                    if let Some(element) = element {
                        yield element;
                    }
                }
            } else {
                let mut source = if inbound {
                    index.get_slot_elements_by_inbound(slot, &key.0).await?
                } else {
                    index.get_slot_elements_by_outbound(slot, &key.0).await?
                };
                let mut references = Vec::new();
                while let Some(element) = source.next().await {
                    let element = element?;
                    cache.check(generation)?;
                    references.push(element.get_reference().clone());
                    yield element;
                }
                cache.put(generation, key, references).await?;
            }
        }))
    }
}

#[async_trait]
impl ElementIndex for CachedElementIndex {
    async fn get_element(
        &self,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        get_element_internal(&self.element_index, &self.element_cache, element_ref).await
    }

    async fn set_element(
        &self,
        element: &Element,
        slot_affinity: &Vec<usize>,
    ) -> Result<(), IndexError> {
        let generation = self.element_cache.generation()?;
        self.element_index
            .set_element(element, slot_affinity)
            .await?;
        self.invalidate(generation).await?;
        self.element_cache
            .put(
                generation,
                element.get_reference().clone(),
                Arc::new(element.clone()),
            )
            .await
    }

    async fn delete_element(&self, element_ref: &ElementReference) -> Result<(), IndexError> {
        let generation = self.element_cache.generation()?;
        self.element_index.delete_element(element_ref).await?;
        self.invalidate(generation).await
    }

    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        let key = (element_ref.clone(), slot);
        let (generation, cached) = self.slot_cache.get(&key).await?;
        let result = match cached {
            Some(false) => None,
            Some(true) => self.get_element(element_ref).await?,
            None => {
                let result = self
                    .element_index
                    .get_slot_element_by_ref(slot, element_ref)
                    .await?;
                self.slot_cache
                    .put(generation, key, result.is_some())
                    .await?;
                result
            }
        };
        self.slot_cache.check(generation)?;
        Ok(result)
    }

    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        inbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        self.adjacent(slot, inbound_ref, true).await
    }

    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        outbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        self.adjacent(slot, outbound_ref, false).await
    }

    async fn clear(&self) -> Result<(), IndexError> {
        let generation = self.element_cache.generation()?;
        self.element_index.clear().await?;
        self.invalidate(generation).await
    }

    async fn set_joins(&self, match_path: &MatchPath, joins: &Vec<Arc<QueryJoin>>) {
        self.element_index.set_joins(match_path, joins).await;
    }
}

async fn get_element_internal(
    index: &Arc<dyn ElementIndex>,
    cache: &GenerationCache<ElementReference, Arc<Element>>,
    reference: &ElementReference,
) -> Result<Option<Arc<Element>>, IndexError> {
    let (generation, cached) = cache.get(reference).await?;
    if cached.is_some() {
        return Ok(cached);
    }
    let result = index.get_element(reference).await?;
    cache.check(generation)?;
    if let Some(element) = &result {
        cache
            .put(generation, reference.clone(), element.clone())
            .await?;
    }
    Ok(result)
}
