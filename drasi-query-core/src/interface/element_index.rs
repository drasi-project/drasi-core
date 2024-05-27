use std::{pin::Pin, sync::Arc};

use async_trait::async_trait;
use futures::Stream;

use crate::models::{Element, ElementReference, ElementTimestamp, TimestampRange};

use super::IndexError;

pub type ElementResult = Result<Arc<Element>, IndexError>;
pub type ElementStream = Pin<Box<dyn Stream<Item = ElementResult> + Send>>;

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
