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

use drasi_core::models::{ElementPropertyMap, ElementValue};
use std::sync::Arc;

/// Builder for creating ElementPropertyMap easily
pub struct PropertyMapBuilder {
    properties: ElementPropertyMap,
}

impl PropertyMapBuilder {
    /// Create a new empty property map builder
    pub fn new() -> Self {
        Self {
            properties: ElementPropertyMap::new(),
        }
    }

    /// Add a string property
    pub fn with_string(mut self, key: impl AsRef<str>, value: impl Into<String>) -> Self {
        self.properties.insert(
            key.as_ref(),
            ElementValue::String(Arc::from(value.into().as_str())),
        );
        self
    }

    /// Add an integer property
    pub fn with_integer(mut self, key: impl AsRef<str>, value: i64) -> Self {
        self.properties
            .insert(key.as_ref(), ElementValue::Integer(value));
        self
    }

    /// Add a float property
    pub fn with_float(mut self, key: impl AsRef<str>, value: f64) -> Self {
        self.properties
            .insert(key.as_ref(), ElementValue::Float(value.into()));
        self
    }

    /// Add a boolean property
    pub fn with_bool(mut self, key: impl AsRef<str>, value: bool) -> Self {
        self.properties
            .insert(key.as_ref(), ElementValue::Bool(value));
        self
    }

    /// Add a null property
    pub fn with_null(mut self, key: impl AsRef<str>) -> Self {
        self.properties.insert(key.as_ref(), ElementValue::Null);
        self
    }

    /// Build the property map
    pub fn build(self) -> ElementPropertyMap {
        self.properties
    }
}

impl Default for PropertyMapBuilder {
    fn default() -> Self {
        Self::new()
    }
}
