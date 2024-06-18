use std::sync::Arc;

use async_stream::stream;
use async_trait::async_trait;
use drasi_core::{
    interface::{ElementArchiveIndex, ElementStream, IndexError},
    models::{Element, ElementReference, ElementTimestamp, TimestampBound, TimestampRange},
};
use prost::Message;
use redis::AsyncCommands;

use crate::storage_models::{StoredElement, StoredElementContainer, StoredElementMetadata};

use super::GarnetElementIndex;

/// Redis key structure:
///
/// archive:{query_id}:{source_id}:{element_id} -> [element (sorted by ts)]
#[async_trait]
impl ElementArchiveIndex for GarnetElementIndex {
    async fn get_element_as_at(
        &self,
        element_ref: &ElementReference,
        time: ElementTimestamp,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        let mut con = self.connection.clone();
        let key = self.key_formatter.get_archive_key(element_ref);
        let result = match con
            .zrevrangebyscore_limit::<String, u64, &str, Vec<Vec<u8>>>(key, time, "-inf", 0, 1)
            .await
        {
            Ok(v) => v,
            Err(e) => return Err(IndexError::other(e)),
        };

        match result.len() {
            0 => Ok(None),
            _ => {
                let element = match result.first() {
                    Some(v) => v,
                    None => return Ok(None),
                };

                let stored_element: StoredElement =
                    match StoredElementContainer::decode(element.as_slice()) {
                        Ok(container) => match container.element {
                            Some(element) => element,
                            None => return Err(IndexError::CorruptedData),
                        },
                        Err(e) => return Err(IndexError::other(e)),
                    };
                Ok(Some(Arc::new(stored_element.into())))
            }
        }
    }

    async fn get_element_versions(
        &self,
        element_ref: &ElementReference,
        range: TimestampRange<ElementTimestamp>,
    ) -> Result<ElementStream, IndexError> {
        let mut con = self.connection.clone();
        let key = self.key_formatter.get_archive_key(element_ref);

        let from = range.from;
        let to = range.to;

        let from_timestamp = match from {
            TimestampBound::Included(from) => from,
            TimestampBound::StartFromPrevious(from) => {
                
                match self.get_element_as_at(element_ref, from).await {
                    Ok(Some(element)) => element.get_effective_from(),
                    Ok(None) => 0,
                    Err(_e) => return Err(IndexError::CorruptedData),
                }
            }
        };
        let stream = stream! {
            let result = con
                .zrangebyscore::<String, u64, u64, Vec<Vec<u8>>>(key, from_timestamp, to)
                .await;

            match result {
                Ok(result) => {
                    for element in result {
                        match StoredElementContainer::decode(element.as_slice()) {
                            Ok(container) => match container.element {
                                Some(element) => yield Ok(Arc::new(element.into())),
                                None => yield Err(IndexError::CorruptedData),
                            }
                            Err(e) => yield Err(IndexError::other(e)),
                        };
                    }
                }
                Err(e) => {
                    yield Err(IndexError::other(e));
                }
            }
        };
        Ok(Box::pin(stream))
    }

    async fn clear(&self) -> Result<(), IndexError> {
        let mut con = self.connection.clone();
        let mut con2 = self.connection.clone();

        let mut keys = match con
            .scan_match::<String, String>(format!("archive:{}:*", self.query_id))
            .await
        {
            Ok(v) => v,
            Err(e) => return Err(IndexError::other(e)),
        };

        while let Some(key) = keys.next_item().await {
            match con2.del::<String, ()>(key).await {
                Ok(_) => (),
                Err(e) => return Err(IndexError::other(e)),
            }
        }
        Ok(())
    }
}

impl GarnetElementIndex {
    pub async fn insert_archive(
        &self,
        metadata: &StoredElementMetadata,
        element: &Vec<Vec<u8>>,
    ) -> Result<(), IndexError> {
        let mut con = self.connection.clone();
        let key = self
            .key_formatter
            .get_stored_archive_key(&metadata.reference);

        if let Err(err) = con
            .zadd::<String, u64, &Vec<Vec<u8>>, isize>(key, element, metadata.effective_from)
            .await
        {
            return Err(IndexError::other(err));
        };

        Ok(())
    }
}
