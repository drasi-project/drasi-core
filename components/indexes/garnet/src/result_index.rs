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

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use async_trait::async_trait;
use drasi_core::{
    evaluation::functions::aggregation::ValueAccumulator,
    interface::{
        AccumulatorIndex, IndexError, LazySortedSetStore, ResultIndex, ResultKey, ResultOwner,
        ResultSequence, ResultSequenceCounter,
    },
};
use hashers::jenkins::spooky_hash::SpookyHasher;
use ordered_float::OrderedFloat;
use redis::{aio::MultiplexedConnection, AsyncCommands};

use crate::{storage_models::StoredValueAccumulator, ClearByPattern};

/// Redis key structure (hash-tagged for cluster compatibility):
/// ari:{<query_id>}:{set_id} -> {value}
/// ari:{<query_id>}:{set_id} -> [sorted value]
/// ari:{<query_id>}:{set_id}:{value} -> count
pub struct GarnetResultIndex {
    query_id: Arc<str>,
    connection: MultiplexedConnection,
}

impl GarnetResultIndex {
    /// Create a new GarnetResultIndex from a shared connection.
    pub fn new(query_id: &str, connection: MultiplexedConnection) -> Self {
        GarnetResultIndex {
            query_id: Arc::from(query_id),
            connection,
        }
    }
}

#[async_trait]
impl AccumulatorIndex for GarnetResultIndex {
    #[tracing::instrument(name = "ari::get", skip_all, err)]
    async fn get(
        &self,
        key: &ResultKey,
        owner: &ResultOwner,
    ) -> Result<Option<ValueAccumulator>, IndexError> {
        let mut con = self.connection.clone();
        let set_id = get_hash_key(owner, key);
        let key = format!("ari:{{{}}}:{}", self.query_id, set_id);
        let result = match con.get::<String, Option<StoredValueAccumulator>>(key).await {
            Ok(v) => v,
            Err(e) => return Err(IndexError::other(e)),
        };

        match result {
            None => Ok(None),
            Some(v) => Ok(Some(v.into())),
        }
    }

    #[tracing::instrument(name = "ari::set", skip_all, err)]
    async fn set(
        &self,
        key: ResultKey,
        owner: ResultOwner,
        value: Option<ValueAccumulator>,
    ) -> Result<(), IndexError> {
        let mut con = self.connection.clone();
        let set_id = get_hash_key(&owner, &key);
        let key = format!("ari:{{{}}}:{}", self.query_id, set_id);

        match value {
            None => match con.del::<String, isize>(key).await {
                Ok(_) => Ok(()),
                Err(e) => Err(IndexError::other(e)),
            },
            Some(v) => {
                match con
                    .set::<String, StoredValueAccumulator, ()>(key, v.into())
                    .await
                {
                    Ok(_) => Ok(()),
                    Err(e) => Err(IndexError::other(e)),
                }
            }
        }
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.connection
            .clear(format!("ari:{{{}}}:*", self.query_id))
            .await
    }
}

#[async_trait]
impl LazySortedSetStore for GarnetResultIndex {
    #[tracing::instrument(name = "lss::get_next", skip_all, err)]
    async fn get_next(
        &self,
        set_id: u64,
        value: Option<OrderedFloat<f64>>,
    ) -> Result<Option<(OrderedFloat<f64>, isize)>, IndexError> {
        let mut con = self.connection.clone();
        let set_key = format!("ari:{{{}}}:{}", self.query_id, set_id);

        let next_value = match value {
            Some(value) => {
                match con
                    .zrangebyscore_limit_withscores::<&str, f64, &str, Vec<(String, f64)>>(
                        &set_key, value.0, "+inf", 1, 1,
                    )
                    .await
                {
                    Ok(v) => {
                        if v.is_empty() {
                            return Ok(None);
                        }
                        (v[0].0.clone(), v[0].1)
                    }
                    Err(e) => return Err(IndexError::other(e)),
                }
            }
            None => {
                match con
                    .zrange_withscores::<&str, Vec<(String, f64)>>(&set_key, 0, 0)
                    .await
                {
                    Ok(v) => {
                        if v.is_empty() {
                            return Ok(None);
                        }
                        (v[0].0.clone(), v[0].1)
                    }
                    Err(e) => return Err(IndexError::other(e)),
                }
            }
        };

        let count = match con
            .get::<String, Option<isize>>(format!("{}:{}", set_key, next_value.0))
            .await
        {
            Ok(v) => v.unwrap_or(0),
            Err(e) => return Err(IndexError::other(e)),
        };

        Ok(Some((next_value.1.into(), count)))
    }

    #[tracing::instrument(name = "lss::get_value_count", skip_all, err)]
    async fn get_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
    ) -> Result<isize, IndexError> {
        let mut con = self.connection.clone();
        let key = format!("ari:{{{}}}:{}:{}", self.query_id, set_id, value.0);

        match con.get::<String, isize>(key).await {
            Ok(v) => Ok(v),
            Err(e) => Err(IndexError::other(e)),
        }
    }

    #[tracing::instrument(name = "lss::increment_value_count", skip_all, err)]
    async fn increment_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
        delta: isize,
    ) -> Result<(), IndexError> {
        let mut con = self.connection.clone();
        let set_key = format!("ari:{{{}}}:{}", self.query_id, set_id);
        let val_key = format!("{}:{}", set_key, value.0);

        let current_count = match con.get::<&str, Option<isize>>(&val_key).await {
            Ok(v) => v.unwrap_or(0),
            Err(e) => return Err(IndexError::other(e)),
        };

        let mut pipeline = redis::pipe();

        if (current_count + delta) == 0 {
            pipeline.del::<&str>(&val_key).ignore();
            pipeline.zrem::<&str, f64>(&set_key, value.0).ignore();
            //todo: add WATCH to ensure that the value is not changed between the get and the del
        } else {
            pipeline.incr::<String, isize>(val_key, delta).ignore();
            pipeline
                .zadd::<String, f64, f64>(set_key, value.0, value.0)
                .ignore();
        }

        if let Err(err) = pipeline.query_async::<_, ()>(&mut con).await {
            return Err(IndexError::other(err));
        }

        Ok(())
    }
}

/// Redis key structure (hash-tagged for cluster compatibility):
/// metadata:{<query_id>}:sequence -> {value}
/// metadata:{<query_id>}:source_change_id -> {value}
#[async_trait]
impl ResultSequenceCounter for GarnetResultIndex {
    async fn apply_sequence(
        &self,
        sequence: u64,
        source_change_id: &str,
    ) -> Result<(), IndexError> {
        let mut con = self.connection.clone();

        let mut pipeline = redis::pipe();
        pipeline
            .set::<String, u64>(format!("metadata:{{{}}}:sequence", self.query_id), sequence)
            .ignore();
        pipeline
            .set::<String, String>(
                format!("metadata:{{{}}}:source_change_id", self.query_id),
                source_change_id.to_string(),
            )
            .ignore();

        if let Err(err) = pipeline.query_async::<_, ()>(&mut con).await {
            return Err(IndexError::other(err));
        }

        Ok(())
    }

    async fn get_sequence(&self) -> Result<ResultSequence, IndexError> {
        let mut con = self.connection.clone();

        let sequence = match con
            .get::<String, Option<u64>>(format!("metadata:{{{}}}:sequence", self.query_id))
            .await
        {
            Ok(v) => v.unwrap_or(0),
            Err(e) => return Err(IndexError::other(e)),
        };

        let source_change_id = match con
            .get::<String, Option<String>>(format!(
                "metadata:{{{}}}:source_change_id",
                self.query_id
            ))
            .await
        {
            Ok(v) => match v {
                Some(v) => v,
                None => "".to_string(),
            },
            Err(e) => return Err(IndexError::other(e)),
        };

        Ok(ResultSequence {
            sequence,
            source_change_id: Arc::from(source_change_id),
        })
    }
}

impl ResultIndex for GarnetResultIndex {}

fn get_hash_key(owner: &ResultOwner, key: &ResultKey) -> u64 {
    let mut hasher = SpookyHasher::default();
    owner.hash(&mut hasher);
    key.hash(&mut hasher);
    hasher.finish()
}
