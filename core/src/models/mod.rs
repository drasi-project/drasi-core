use std::fmt::{Display, Formatter};
use std::sync::Arc;

use serde_json::{Map, Value};
use thiserror::Error;

mod element;
mod element_value;
mod source_change;
mod timestamp_range;

pub use element::{Element, ElementMetadata, ElementReference, ElementTimestamp};
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

#[derive(Debug, Clone)]
pub struct SourceMiddlewareConfig {
    pub kind: Arc<str>,
    pub name: Arc<str>,
    pub config: Map<String, Value>,
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
