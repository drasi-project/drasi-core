use std::sync::Arc;

use async_trait::async_trait;
use drasi_query_core::{
    interface::{FutureElementRef, FutureQueue, IndexError, PushType},
    models::{ElementReference, ElementTimestamp},
};
use redis::{aio::MultiplexedConnection, AsyncCommands};

use crate::storage_models::{StoredFutureElementRef, StoredFutureElementRefWithContext};

/// Redis key structure:
///
/// fqi:{query_id} -> {full future_ref}  (sorted set by due_time)
/// fqi:{query_id}:{position_in_query}:{group_signature} -> {future_ref}  (set)
pub struct GarnetFutureQueue {
    query_id: Arc<str>,
    connection: MultiplexedConnection,
}

impl GarnetFutureQueue {
    pub async fn connect(query_id: &str, url: &str) -> Result<Self, IndexError> {
        let client = match redis::Client::open(url) {
            Ok(client) => client,
            Err(e) => return Err(IndexError::connection_failed(e)),
        };

        let connection = match client.get_multiplexed_async_connection().await {
            Ok(con) => con,
            Err(e) => return Err(IndexError::connection_failed(e)),
        };

        Ok(Self {
            query_id: Arc::from(query_id),
            connection,
        })
    }

    fn get_queue_key(&self) -> String {
        format!("fqi:{}", self.query_id)
    }

    fn get_index_key(&self, position_in_query: usize, group_signature: u64) -> String {
        format!(
            "fqi:{}:{}:{}",
            self.query_id, position_in_query, group_signature
        )
    }
}

#[async_trait]
impl FutureQueue for GarnetFutureQueue {
    async fn push(
        &self,
        push_type: PushType,
        position_in_query: usize,
        group_signature: u64,
        element_ref: &ElementReference,
        original_time: ElementTimestamp,
        due_time: ElementTimestamp,
    ) -> Result<bool, IndexError> {
        let mut con = self.connection.clone();

        let future_ref = StoredFutureElementRef {
            element_ref: element_ref.into(),
            original_time,
            due_time,
        };

        let should_push = {
            match push_type {
                PushType::Always => true,
                PushType::IfNotExists => {
                    let exists: bool = match con
                        .exists(self.get_index_key(position_in_query, group_signature))
                        .await
                    {
                        Ok(v) => v,
                        Err(e) => return Err(IndexError::other(e)),
                    };
                    !exists
                }
                PushType::Overwrite => {
                    self.remove(position_in_query, group_signature).await?;
                    true
                }
            }
        };

        if should_push {
            let _: usize = match con
                .sadd(
                    self.get_index_key(position_in_query, group_signature),
                    &future_ref,
                )
                .await
            {
                Ok(v) => v,
                Err(e) => return Err(IndexError::other(e)),
            };

            let _: usize = match con
                .zadd(
                    self.get_queue_key(),
                    StoredFutureElementRefWithContext {
                        future_ref,
                        position_in_query: position_in_query as u32,
                        group_signature,
                    },
                    due_time,
                )
                .await
            {
                Ok(v) => v,
                Err(e) => return Err(IndexError::other(e)),
            };
        }
        Ok(should_push)
    }

    async fn remove(
        &self,
        position_in_query: usize,
        group_signature: u64,
    ) -> Result<(), IndexError> {
        let mut con = self.connection.clone();

        let members: Vec<StoredFutureElementRef> = match con
            .smembers(self.get_index_key(position_in_query, group_signature))
            .await
        {
            Ok(v) => v,
            Err(e) => return Err(IndexError::other(e)),
        };

        let members = members
            .into_iter()
            .map(|v| StoredFutureElementRefWithContext {
                future_ref: v,
                position_in_query: position_in_query as u32,
                group_signature,
            })
            .collect::<Vec<StoredFutureElementRefWithContext>>();

        if !members.is_empty() {
            let _: usize = match con.zrem(self.get_queue_key(), members).await {
                Ok(v) => v,
                Err(e) => return Err(IndexError::other(e)),
            };
        }

        let _: usize = match con
            .del(self.get_index_key(position_in_query, group_signature))
            .await
        {
            Ok(v) => v,
            Err(e) => return Err(IndexError::other(e)),
        };

        Ok(())
    }

    async fn pop(&self) -> Result<Option<FutureElementRef>, IndexError> {
        let mut con = self.connection.clone();
        let result: Vec<(StoredFutureElementRefWithContext, f64)> =
            match con.zpopmin(self.get_queue_key(), 1).await {
                Ok(v) => v,
                Err(e) => return Err(IndexError::other(e)),
            };

        match result.first() {
            None => Ok(None),
            Some((v, _)) => {
                let _: usize = match con
                    .srem(
                        self.get_index_key(v.position_in_query as usize, v.group_signature),
                        &v.future_ref,
                    )
                    .await
                {
                    Ok(v) => v,
                    Err(e) => return Err(IndexError::other(e)),
                };

                let result = v.into();
                Ok(Some(result))
            }
        }
    }

    async fn peek_due_time(&self) -> Result<Option<ElementTimestamp>, IndexError> {
        let mut con = self.connection.clone();
        let result: Vec<(StoredFutureElementRefWithContext, f64)> = match con
            .zrangebyscore_limit_withscores(self.get_queue_key(), 0, "+inf", 0, 1)
            .await
        {
            Ok(v) => v,
            Err(e) => return Err(IndexError::other(e)),
        };

        if result.is_empty() {
            Ok(None)
        } else {
            Ok(Some(result[0].1 as u64))
        }
    }

    async fn clear(&self) -> Result<(), IndexError> {
        let mut con = self.connection.clone();
        let mut con2 = self.connection.clone();

        match con2
            .del::<String, ()>(format!("fqi:{}", self.query_id))
            .await
        {
            Ok(_) => (),
            Err(e) => return Err(IndexError::other(e)),
        };

        let mut keys = match con
            .scan_match::<String, String>(format!("fqi:{}:*", self.query_id))
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
