use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::{
    interface::{FutureElementRef, FutureQueue, IndexError, PushType},
    models::{ElementReference, ElementTimestamp},
};

enum HeadItemShadow {
    Known(Option<ElementTimestamp>),
    Unknown,
}

pub struct ShadowedFutureQueue {
    inner: Arc<dyn FutureQueue>,
    head_shadow: RwLock<HeadItemShadow>,
}

impl ShadowedFutureQueue {
    pub fn new(inner: Arc<dyn FutureQueue>) -> Self {
        Self {
            inner,
            head_shadow: RwLock::new(HeadItemShadow::Unknown),
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

        if push_type == PushType::Overwrite {
            let mut head_shadow = self.head_shadow.write().await;
            *head_shadow = HeadItemShadow::Unknown;
            return Ok(result);
        }

        if result {
            let mut head_shadow = self.head_shadow.write().await;

            match *head_shadow {
                HeadItemShadow::Known(Some(head_due_time)) => {
                    if due_time < head_due_time {
                        *head_shadow = HeadItemShadow::Known(Some(due_time));
                    }
                }
                HeadItemShadow::Known(None) => {
                    *head_shadow = HeadItemShadow::Known(Some(due_time));
                }
                HeadItemShadow::Unknown => {}
            }
        }

        Ok(result)
    }

    async fn remove(
        &self,
        position_in_query: usize,
        group_signature: u64,
    ) -> Result<(), IndexError> {
        let mut shadow = self.head_shadow.write().await;
        *shadow = HeadItemShadow::Unknown;
        self.inner.remove(position_in_query, group_signature).await
    }

    async fn pop(&self) -> Result<Option<FutureElementRef>, IndexError> {
        let mut shadow = self.head_shadow.write().await;
        *shadow = HeadItemShadow::Unknown;
        self.inner.pop().await
    }

    async fn peek_due_time(&self) -> Result<Option<ElementTimestamp>, IndexError> {
        let mut shadow = self.head_shadow.write().await;
        match *shadow {
            HeadItemShadow::Known(Some(head_due_time)) => Ok(Some(head_due_time)),
            HeadItemShadow::Known(None) => Ok(None),
            HeadItemShadow::Unknown => {
                let result = self.inner.peek_due_time().await?;
                *shadow = HeadItemShadow::Known(result);
                Ok(result)
            }
        }
    }

    async fn clear(&self) -> Result<(), IndexError> {
        let mut shadow = self.head_shadow.write().await;
        *shadow = HeadItemShadow::Unknown;
        self.inner.clear().await
    }
}
