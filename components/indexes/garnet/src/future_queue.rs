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

use async_trait::async_trait;
use drasi_core::{
    interface::{FutureElementRef, FutureQueue, IndexError, PushType},
    models::{ElementReference, ElementTimestamp},
};
use redis::{aio::MultiplexedConnection, AsyncCommands};

use crate::{
    storage_models::{StoredFutureElementRef, StoredFutureElementRefWithContext},
    ClearByPattern,
};

/// Redis key structure (hash-tagged for cluster compatibility):
///
/// fqi:{<query_id>} -> {full future_ref}  (sorted set by due_time)
/// fqi:{<query_id>}:{position_in_query}:{group_signature} -> {future_ref}  (set)
pub struct GarnetFutureQueue {
    query_id: Arc<str>,
    connection: MultiplexedConnection,
}

impl GarnetFutureQueue {
    /// Create a new GarnetFutureQueue from a shared connection.
    pub fn new(query_id: &str, connection: MultiplexedConnection) -> Self {
        Self {
            query_id: Arc::from(query_id),
            connection,
        }
    }

    fn get_queue_key(&self) -> String {
        format!("fqi:{{{}}}", self.query_id)
    }

    fn get_index_key(&self, position_in_query: usize, group_signature: u64) -> String {
        format!(
            "fqi:{{{}}}:{}:{}",
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
        // Delete the main queue sorted set key (not matched by the :* pattern below)
        let mut con = self.connection.clone();
        let _: () = con
            .del(self.get_queue_key())
            .await
            .map_err(IndexError::other)?;
        // Delete secondary index keys
        self.connection
            .clear(format!("fqi:{{{}}}:*", self.query_id))
            .await
    }
}
