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

use prost::Message;
use redis::{FromRedisValue, RedisError, ToRedisArgs};

use super::{
    StoredElementContainer, StoredElementReference, StoredFutureElementRef,
    StoredFutureElementRefWithContext, StoredValueAccumulator, StoredValueAccumulatorContainer,
};

impl ToRedisArgs for &StoredElementReference {
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + redis::RedisWrite,
    {
        let data = self.encode_to_vec();
        out.write_arg(data.as_slice());
    }
}

impl FromRedisValue for StoredElementReference {
    fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
        match v {
            redis::Value::Data(data) => match StoredElementReference::decode(data.as_slice()) {
                Ok(v) => Ok(v),
                Err(_e) => Err(RedisError::from((
                    redis::ErrorKind::TypeError,
                    "Error decoding element reference",
                ))),
            },
            _ => Err(redis::RedisError::from((
                redis::ErrorKind::TypeError,
                "Invalid Response type for element reference",
            ))),
        }
    }
}

impl ToRedisArgs for StoredElementContainer {
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + redis::RedisWrite,
    {
        let data = self.encode_to_vec();
        out.write_arg(data.as_slice());
    }
}

impl FromRedisValue for StoredElementContainer {
    fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
        match v {
            redis::Value::Data(data) => {
                let container = match StoredElementContainer::decode(data.as_slice()) {
                    Ok(v) => v,
                    Err(_e) => {
                        return Err(RedisError::from((
                            redis::ErrorKind::TypeError,
                            "Error decoding element",
                        )))
                    }
                };
                Ok(container)
            }
            _ => Err(redis::RedisError::from((
                redis::ErrorKind::TypeError,
                "Invalid Response type for element container",
            ))),
        }
    }
}

impl ToRedisArgs for StoredValueAccumulator {
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + redis::RedisWrite,
    {
        let container = StoredValueAccumulatorContainer {
            value: Some(self.clone()),
        };
        let data = container.encode_to_vec();
        out.write_arg(data.as_slice());
    }
}

impl FromRedisValue for StoredValueAccumulator {
    fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
        match v {
            redis::Value::Data(data) => {
                let container = match StoredValueAccumulatorContainer::decode(data.as_slice()) {
                    Ok(v) => v,
                    Err(_e) => {
                        return Err(RedisError::from((
                            redis::ErrorKind::TypeError,
                            "Error decoding element",
                        )))
                    }
                };
                match container.value {
                    Some(v) => Ok(v),
                    None => Err(RedisError::from((
                        redis::ErrorKind::TypeError,
                        "Invalid Response type for value accumulator",
                    ))),
                }
            }
            _ => Err(redis::RedisError::from((
                redis::ErrorKind::TypeError,
                "Invalid Response type for value accumulator",
            ))),
        }
    }
}

#[allow(clippy::unwrap_used)]
impl FromRedisValue for StoredFutureElementRef {
    fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
        match v {
            redis::Value::Data(data) => {
                let future_ref = StoredFutureElementRef::decode(data.as_slice()).unwrap();
                Ok(future_ref)
            }
            _ => Err(redis::RedisError::from((
                redis::ErrorKind::TypeError,
                "Invalid Response type for future element reference",
            ))),
        }
    }
}

impl ToRedisArgs for &StoredFutureElementRef {
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + redis::RedisWrite,
    {
        let data = self.encode_to_vec();
        out.write_arg(data.as_slice());
    }
}

#[allow(clippy::unwrap_used)]
impl FromRedisValue for StoredFutureElementRefWithContext {
    fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
        match v {
            redis::Value::Data(data) => {
                let future_ref =
                    StoredFutureElementRefWithContext::decode(data.as_slice()).unwrap();
                Ok(future_ref)
            }
            _ => Err(redis::RedisError::from((
                redis::ErrorKind::TypeError,
                "Invalid Response type for future element reference",
            ))),
        }
    }
}

impl ToRedisArgs for StoredFutureElementRefWithContext {
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + redis::RedisWrite,
    {
        let data = self.encode_to_vec();
        out.write_arg(data.as_slice());
    }
}
