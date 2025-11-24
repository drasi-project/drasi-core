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

/// Type-safe property map builder for component configuration
///
/// `Properties` provides an ergonomic fluent API for building property maps used to configure
/// sources, queries, and reactions. It wraps a `HashMap<String, serde_json::Value>` with
/// type-safe setters.
///
/// # Supported Types
///
/// - **String**: Text values via `with_string()`
/// - **Integer**: 64-bit signed integers via `with_int()`
/// - **Boolean**: true/false values via `with_bool()`
/// - **Custom**: Any `serde_json::Value` via `with_value()`
///
/// # Thread Safety
///
/// `Properties` is `Clone` and can be used to create multiple property maps.
///
/// # Examples
///
/// ## PostgreSQL Source Properties
///
/// ```
/// use drasi_lib::Properties;
///
/// let props = Properties::new()
///     .with_string("host", "localhost")
///     .with_int("port", 5432)
///     .with_string("database", "mydb")
///     .with_string("user", "postgres")
///     .with_string("password", "secret")
///     .with_bool("ssl_enabled", true);
/// ```
///
/// ## HTTP Reaction Properties
///
/// ```
/// use drasi_lib::Properties;
///
/// let props = Properties::new()
///     .with_string("base_url", "https://api.example.com/webhook")
///     .with_string("token", "secret-token")
///     .with_int("timeout_ms", 10000);
/// ```
///
/// ## Using with Component Builders
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, Properties};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(
///         Source::postgres("db")
///             .with_properties(
///                 Properties::new()
///                     .with_string("host", "localhost")
///                     .with_int("port", 5432)
///                     .with_string("database", "orders")
///             )
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct Properties {
    inner: HashMap<String, Value>,
}

impl Properties {
    /// Create a new empty Properties builder
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_lib::Properties;
    ///
    /// let props = Properties::new();
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a string property
    ///
    /// Adds a string key-value pair to the property map.
    ///
    /// # Arguments
    ///
    /// * `key` - Property name
    /// * `value` - String value
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_lib::Properties;
    ///
    /// let props = Properties::new()
    ///     .with_string("host", "localhost")
    ///     .with_string("database", "mydb");
    /// ```
    pub fn with_string(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.inner.insert(key.into(), Value::String(value.into()));
        self
    }

    /// Set an integer property (64-bit signed)
    ///
    /// Adds an integer key-value pair to the property map.
    ///
    /// # Arguments
    ///
    /// * `key` - Property name
    /// * `value` - Integer value (i64)
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_lib::Properties;
    ///
    /// let props = Properties::new()
    ///     .with_int("port", 5432)
    ///     .with_int("timeout_ms", 30000);
    /// ```
    pub fn with_int(mut self, key: impl Into<String>, value: i64) -> Self {
        self.inner.insert(key.into(), Value::Number(value.into()));
        self
    }

    /// Set a boolean property
    ///
    /// Adds a boolean key-value pair to the property map.
    ///
    /// # Arguments
    ///
    /// * `key` - Property name
    /// * `value` - Boolean value
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_lib::Properties;
    ///
    /// let props = Properties::new()
    ///     .with_bool("ssl_enabled", true)
    ///     .with_bool("auto_reconnect", false);
    /// ```
    pub fn with_bool(mut self, key: impl Into<String>, value: bool) -> Self {
        self.inner.insert(key.into(), Value::Bool(value));
        self
    }

    /// Set a property using a raw JSON value
    ///
    /// Adds a key-value pair using a raw `serde_json::Value`. This allows setting complex
    /// values like arrays or nested objects.
    ///
    /// # Arguments
    ///
    /// * `key` - Property name
    /// * `value` - JSON value
    ///
    /// # Examples
    ///
    /// ## Array Property
    ///
    /// ```
    /// use drasi_lib::Properties;
    /// use serde_json::json;
    ///
    /// let props = Properties::new()
    ///     .with_value("tables", json!(["orders", "customers", "products"]));
    /// ```
    ///
    /// ## Nested Object
    ///
    /// ```
    /// use drasi_lib::Properties;
    /// use serde_json::json;
    ///
    /// let props = Properties::new()
    ///     .with_value("credentials", json!({
    ///         "username": "admin",
    ///         "password": "secret"
    ///     }));
    /// ```
    pub fn with_value(mut self, key: impl Into<String>, value: Value) -> Self {
        self.inner.insert(key.into(), value);
        self
    }

    /// Build into a HashMap
    ///
    /// Consumes the builder and returns the underlying `HashMap<String, serde_json::Value>`.
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_lib::Properties;
    ///
    /// let props = Properties::new()
    ///     .with_string("key", "value")
    ///     .build();
    ///
    /// assert_eq!(props.len(), 1);
    /// ```
    pub fn build(self) -> HashMap<String, Value> {
        self.inner
    }

    /// Create a Properties builder from an existing HashMap
    ///
    /// Wraps an existing property map in a builder for further modification.
    ///
    /// # Arguments
    ///
    /// * `map` - Existing property map
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_lib::Properties;
    /// use std::collections::HashMap;
    /// use serde_json::json;
    ///
    /// let mut map = HashMap::new();
    /// map.insert("host".to_string(), json!("localhost"));
    ///
    /// let props = Properties::from_map(map)
    ///     .with_int("port", 5432);  // Add more properties
    /// ```
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
