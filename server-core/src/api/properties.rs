// Copyright 2025 The Drasi Authors.
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

//! Type-safe property builders for component configuration

use serde_json::Value;
use std::collections::HashMap;

/// Type-safe property map builder
#[derive(Debug, Clone, Default)]
pub struct Properties {
    inner: HashMap<String, Value>,
}

impl Properties {
    /// Create a new empty Properties
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a string property
    pub fn with_string(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.inner.insert(key.into(), Value::String(value.into()));
        self
    }

    /// Set an integer property
    pub fn with_int(mut self, key: impl Into<String>, value: i64) -> Self {
        self.inner.insert(key.into(), Value::Number(value.into()));
        self
    }

    /// Set a boolean property
    pub fn with_bool(mut self, key: impl Into<String>, value: bool) -> Self {
        self.inner.insert(key.into(), Value::Bool(value));
        self
    }

    /// Set a JSON value property
    pub fn with_value(mut self, key: impl Into<String>, value: Value) -> Self {
        self.inner.insert(key.into(), value);
        self
    }

    /// Build into HashMap
    pub fn build(self) -> HashMap<String, Value> {
        self.inner
    }

    /// Create from existing HashMap
    pub fn from_map(map: HashMap<String, Value>) -> Self {
        Self { inner: map }
    }
}

impl From<HashMap<String, Value>> for Properties {
    fn from(map: HashMap<String, Value>) -> Self {
        Self { inner: map }
    }
}

impl From<Properties> for HashMap<String, Value> {
    fn from(props: Properties) -> Self {
        props.inner
    }
}
