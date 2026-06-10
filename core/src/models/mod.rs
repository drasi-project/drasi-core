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

use std::fmt::{Display, Formatter};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

mod element;
mod element_value;
mod source_change;
mod timestamp_range;

pub use element::{
    validate_effective_from, Element, ElementMetadata, ElementReference, ElementTimestamp,
    MAX_REASONABLE_MILLIS_TIMESTAMP,
};
pub use element_value::ElementPropertyMap;
pub use element_value::ElementValue;
pub use source_change::SourceChange;
pub use timestamp_range::{TimestampBound, TimestampRange};

#[derive(Debug, Error)]
pub struct ConversionError {}

impl Display for ConversionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cannot convert")
    }
}

#[derive(Debug, Clone)]
pub struct QuerySourceElement {
    pub source_label: String,
}

#[derive(Debug, Clone)]
pub struct QuerySubscription {
    pub id: Arc<str>,
    pub nodes: Vec<QuerySourceElement>,
    pub relations: Vec<QuerySourceElement>,
    pub pipeline: Vec<Arc<str>>,
}

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct QueryJoinKey {
    pub label: String,
    pub property: String,
}

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct QueryJoin {
    pub id: String,
    pub keys: Vec<QueryJoinKey>,
}

#[derive(Debug, Clone)]
pub struct QueryConfig {
    pub mode: String,
    pub query: String,
    pub sources: QuerySources,
    pub storage_profile: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QuerySources {
    pub subscriptions: Vec<QuerySubscription>,
    pub joins: Vec<QueryJoin>,
    pub middleware: Vec<SourceMiddlewareConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceMiddlewareConfig {
    #[serde(
        serialize_with = "serialize_arc_str",
        deserialize_with = "deserialize_arc_str"
    )]
    pub kind: Arc<str>,
    #[serde(
        serialize_with = "serialize_arc_str",
        deserialize_with = "deserialize_arc_str"
    )]
    pub name: Arc<str>,
    pub config: Map<String, Value>,
}

fn serialize_arc_str<S>(arc: &Arc<str>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(arc)
}

fn deserialize_arc_str<'de, D>(deserializer: D) -> Result<Arc<str>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Ok(Arc::from(s.as_str()))
}

impl SourceMiddlewareConfig {
    pub fn new(kind: &str, name: &str, config: Map<String, Value>) -> Self {
        SourceMiddlewareConfig {
            kind: Arc::from(kind),
            name: Arc::from(name),
            config,
        }
    }
}
