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

/// Builder for creating graph element property maps
///
/// `PropertyMapBuilder` provides a fluent API for constructing [`ElementPropertyMap`] instances
/// used when inserting or updating nodes and relationships in the graph.
///
/// # Supported Property Types
///
/// - **String**: Text values
/// - **Integer**: 64-bit signed integers
/// - **Float**: 64-bit floating point numbers
/// - **Boolean**: true/false values
/// - **Null**: Explicit null values
///
/// # Thread Safety
///
/// `PropertyMapBuilder` is not thread-safe. Create a new builder for each thread.
///
/// # Examples
///
/// ## Basic Usage
///
/// ```
/// use drasi_plugin_application::PropertyMapBuilder;
///
/// let properties = PropertyMapBuilder::new()
///     .with_string("name", "Alice")
///     .with_integer("age", 30)
///     .with_bool("active", true)
///     .with_float("score", 95.5)
///     .build();
/// ```
///
/// ## With Application Source
///
/// ```no_run
/// use drasi_plugin_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
/// use std::collections::HashMap;
/// use tokio::sync::mpsc;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let (event_tx, _event_rx) = mpsc::channel(100);
/// let config = ApplicationSourceConfig { properties: HashMap::new() };
/// let (source, handle) = ApplicationSource::new("events", config, event_tx)?;
///
/// // Create a user node with multiple properties
/// let properties = PropertyMapBuilder::new()
///     .with_string("username", "alice")
///     .with_string("email", "alice@example.com")
///     .with_integer("age", 30)
///     .with_bool("verified", true)
///     .with_float("rating", 4.8)
///     .build();
///
/// handle.send_node_insert("user-1", vec!["User"], properties).await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Null Values
///
/// ```
/// use drasi_plugin_application::PropertyMapBuilder;
///
/// let properties = PropertyMapBuilder::new()
///     .with_string("name", "Bob")
///     .with_null("middle_name")  // Explicit null
///     .build();
/// ```
pub struct PropertyMapBuilder {
    properties: ElementPropertyMap,
}

impl PropertyMapBuilder {
    /// Create a new empty property map builder
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_plugin_application::PropertyMapBuilder;
    ///
    /// let builder = PropertyMapBuilder::new();
    /// let properties = builder.build();
    /// ```
    pub fn new() -> Self {
        Self {
            properties: ElementPropertyMap::new(),
        }
    }

    /// Add a string property
    ///
    /// # Arguments
    ///
    /// * `key` - Property name
    /// * `value` - String value (converted to `Arc<str>` internally)
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_plugin_application::PropertyMapBuilder;
    ///
    /// let properties = PropertyMapBuilder::new()
    ///     .with_string("name", "Alice")
    ///     .with_string("email", "alice@example.com")
    ///     .build();
    /// ```
    pub fn with_string(mut self, key: impl AsRef<str>, value: impl Into<String>) -> Self {
        self.properties.insert(
            key.as_ref(),
            ElementValue::String(Arc::from(value.into().as_str())),
        );
        self
    }

    /// Add an integer property (64-bit signed)
    ///
    /// # Arguments
    ///
    /// * `key` - Property name
    /// * `value` - Integer value (i64)
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_plugin_application::PropertyMapBuilder;
    ///
    /// let properties = PropertyMapBuilder::new()
    ///     .with_integer("age", 30)
    ///     .with_integer("count", 1000)
    ///     .build();
    /// ```
    pub fn with_integer(mut self, key: impl AsRef<str>, value: i64) -> Self {
        self.properties
            .insert(key.as_ref(), ElementValue::Integer(value));
        self
    }

    /// Add a floating-point property (64-bit)
    ///
    /// # Arguments
    ///
    /// * `key` - Property name
    /// * `value` - Float value (f64)
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_plugin_application::PropertyMapBuilder;
    ///
    /// let properties = PropertyMapBuilder::new()
    ///     .with_float("rating", 4.5)
    ///     .with_float("price", 29.99)
    ///     .build();
    /// ```
    pub fn with_float(mut self, key: impl AsRef<str>, value: f64) -> Self {
        self.properties
            .insert(key.as_ref(), ElementValue::Float(value.into()));
        self
    }

    /// Add a boolean property
    ///
    /// # Arguments
    ///
    /// * `key` - Property name
    /// * `value` - Boolean value
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_plugin_application::PropertyMapBuilder;
    ///
    /// let properties = PropertyMapBuilder::new()
    ///     .with_bool("active", true)
    ///     .with_bool("verified", false)
    ///     .build();
    /// ```
    pub fn with_bool(mut self, key: impl AsRef<str>, value: bool) -> Self {
        self.properties
            .insert(key.as_ref(), ElementValue::Bool(value));
        self
    }

    /// Add an explicit null property
    ///
    /// Use this to explicitly set a property to null, which is different from
    /// not having the property at all.
    ///
    /// # Arguments
    ///
    /// * `key` - Property name
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_plugin_application::PropertyMapBuilder;
    ///
    /// let properties = PropertyMapBuilder::new()
    ///     .with_string("first_name", "Alice")
    ///     .with_null("middle_name")  // Explicitly null
    ///     .with_string("last_name", "Smith")
    ///     .build();
    /// ```
    pub fn with_null(mut self, key: impl AsRef<str>) -> Self {
        self.properties.insert(key.as_ref(), ElementValue::Null);
        self
    }

    /// Build the final property map
    ///
    /// Consumes the builder and returns the constructed [`ElementPropertyMap`].
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_plugin_application::PropertyMapBuilder;
    ///
    /// let properties = PropertyMapBuilder::new()
    ///     .with_string("name", "Alice")
    ///     .with_integer("age", 30)
    ///     .build();
    ///
    /// // properties can now be used with send_node_insert, send_node_update, etc.
    /// ```
    pub fn build(self) -> ElementPropertyMap {
        self.properties
    }
}

impl Default for PropertyMapBuilder {
    fn default() -> Self {
        Self::new()
    }
}
