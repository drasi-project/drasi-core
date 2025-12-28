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

//! Common template configuration types for reactions.
//!
//! This module provides shared template structures that can be used across
//! different reaction implementations (SSE, Logger, StoredProc, etc.) to support
//! Handlebars template syntax for formatting outputs based on operation types.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Specification for template-based output.
///
/// This type is used to configure templates for different operation types (added, updated, deleted).
/// Template fields support Handlebars template syntax for dynamic content generation.
///
/// # Type Parameter
///
/// - `T` - Optional extension type for reaction-specific fields. Use `()` (default) if no extensions needed.
///
/// # Template Variables
///
/// Templates have access to the following variables:
/// - `after` - The data after the change (available for ADD and UPDATE)
/// - `before` - The data before the change (available for UPDATE and DELETE)
/// - `data` - The raw data field (available for UPDATE)
/// - `query_name` - The name of the query that produced the result
/// - `operation` - The operation type ("ADD", "UPDATE", or "DELETE")
/// - `timestamp` - The timestamp of the event (if available)
///
/// # Example (Basic)
///
/// ```rust
/// use drasi_lib::reactions::common::TemplateSpec;
///
/// let spec: TemplateSpec = TemplateSpec {
///     template: "[NEW] ID: {{after.id}}, Name: {{after.name}}".to_string(),
///     extension: (),
/// };
/// ```
///
/// # Example (With Extensions)
///
/// ```rust
/// use drasi_lib::reactions::common::TemplateSpec;
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
/// struct MyExtension {
///     custom_field: String,
/// }
///
/// let spec: TemplateSpec<MyExtension> = TemplateSpec {
///     template: "[NEW] {{after.id}}".to_string(),
///     extension: MyExtension {
///         custom_field: "value".to_string(),
///     },
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(bound(deserialize = "T: Deserialize<'de> + Default"))]
pub struct TemplateSpec<T = ()>
where
    T: Default,
{
    /// Output template as a Handlebars template.
    /// If empty, the reaction may use a default format (typically raw JSON).
    #[serde(default)]
    pub template: String,

    /// Extension data for reaction-specific fields.
    /// This is flattened in the JSON representation, so extension fields appear at the same level as `template`.
    #[serde(flatten, default)]
    pub extension: T,
}

impl TemplateSpec {
    /// Create a new TemplateSpec with no extensions (for reactions that don't need custom fields).
    ///
    /// # Example
    ///
    /// ```rust
    /// use drasi_lib::reactions::common::TemplateSpec;
    ///
    /// let spec = TemplateSpec::new("{{after.id}}");
    /// ```
    pub fn new(template: impl Into<String>) -> Self {
        Self {
            template: template.into(),
            extension: (),
        }
    }
}

impl<T: Default> TemplateSpec<T> {
    /// Create a new TemplateSpec with a custom extension.
    ///
    /// # Example
    ///
    /// ```rust
    /// use drasi_lib::reactions::common::TemplateSpec;
    /// use serde::{Deserialize, Serialize};
    ///
    /// #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
    /// struct MyExtension {
    ///     custom_field: String,
    /// }
    ///
    /// let spec = TemplateSpec::with_extension(
    ///     "{{after.id}}",
    ///     MyExtension { custom_field: "value".to_string() }
    /// );
    /// ```
    pub fn with_extension(template: impl Into<String>, extension: T) -> Self {
        Self {
            template: template.into(),
            extension,
        }
    }
}

impl<T: Default> Default for TemplateSpec<T> {
    /// Create a default TemplateSpec with empty template and default extension.
    ///
    /// This allows using the `..Default::default()` syntax:
    ///
    /// # Example
    ///
    /// ```rust
    /// use drasi_lib::reactions::common::TemplateSpec;
    ///
    /// let spec = TemplateSpec {
    ///     template: "{{after.id}}".to_string(),
    ///     ..Default::default()
    /// };
    /// ```
    fn default() -> Self {
        Self {
            template: String::new(),
            extension: T::default(),
        }
    }
}

/// Configuration for query-specific template-based output.
///
/// Defines different template specifications for each operation type (added, updated, deleted).
/// Each operation type can have its own formatting template.
///
/// # Type Parameter
///
/// - `T` - Optional extension type for reaction-specific fields. Use `()` (default) if no extensions needed.
///
/// # Example
///
/// ```rust
/// use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
///
/// let config: QueryConfig = QueryConfig {
///     added: Some(TemplateSpec {
///         template: "[ADD] {{after.id}}".to_string(),
///         extension: (),
///     }),
///     updated: Some(TemplateSpec {
///         template: "[UPD] {{after.id}}".to_string(),
///         extension: (),
///     }),
///     deleted: Some(TemplateSpec {
///         template: "[DEL] {{before.id}}".to_string(),
///         extension: (),
///     }),
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(bound(deserialize = "T: Deserialize<'de> + Default"))]
pub struct QueryConfig<T = ()>
where
    T: Default,
{
    /// Template specification for ADD operations (new rows in query results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<TemplateSpec<T>>,

    /// Template specification for UPDATE operations (modified rows in query results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<TemplateSpec<T>>,

    /// Template specification for DELETE operations (removed rows from query results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<TemplateSpec<T>>,
}

/// Template routing configuration for reactions.
///
/// Provides a way to configure templates at two levels:
/// 1. **Default template**: Applied to all queries unless overridden
/// 2. **Per-query templates**: Override default for specific queries
///
/// This trait can be used by any reaction that needs template-based routing.
///
/// # Type Parameter
///
/// - `T` - Optional extension type for reaction-specific fields. Use `()` (default) if no extensions needed.
///
/// # Example
///
/// ```rust
/// use std::collections::HashMap;
/// use drasi_lib::reactions::common::{TemplateRouting, QueryConfig, TemplateSpec};
///
/// #[derive(Debug, Clone)]
/// struct MyReactionConfig {
///     routes: HashMap<String, QueryConfig>,
///     default_template: Option<QueryConfig>,
/// }
///
/// impl TemplateRouting for MyReactionConfig {
///     fn routes(&self) -> &HashMap<String, QueryConfig> {
///         &self.routes
///     }
///
///     fn default_template(&self) -> Option<&QueryConfig> {
///         self.default_template.as_ref()
///     }
/// }
/// ```
pub trait TemplateRouting<T = ()>
where
    T: Default,
{
    /// Get the query-specific template routes
    fn routes(&self) -> &HashMap<String, QueryConfig<T>>;

    /// Get the default template configuration
    fn default_template(&self) -> Option<&QueryConfig<T>>;

    /// Get the template spec for a specific query and operation type
    fn get_template_spec(
        &self,
        query_id: &str,
        operation: OperationType,
    ) -> Option<&TemplateSpec<T>> {
        // First check query-specific routes
        if let Some(query_config) = self.routes().get(query_id) {
            if let Some(spec) = Self::get_spec_from_config(query_config, operation) {
                return Some(spec);
            }
        }

        // Fall back to default template
        if let Some(default_config) = self.default_template() {
            return Self::get_spec_from_config(default_config, operation);
        }

        None
    }

    /// Helper to get spec from a QueryConfig based on operation type
    fn get_spec_from_config(
        config: &QueryConfig<T>,
        operation: OperationType,
    ) -> Option<&TemplateSpec<T>> {
        match operation {
            OperationType::Add => config.added.as_ref(),
            OperationType::Update => config.updated.as_ref(),
            OperationType::Delete => config.deleted.as_ref(),
        }
    }
}

/// Operation type for query results
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationType {
    /// Add operation (new row)
    Add,
    /// Update operation (modified row)
    Update,
    /// Delete operation (removed row)
    Delete,
}

impl std::str::FromStr for OperationType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "add" => Ok(Self::Add),
            "update" => Ok(Self::Update),
            "delete" => Ok(Self::Delete),
            _ => Err(format!("Invalid operation type: {}", s)),
        }
    }
}

impl OperationType {
    /// Convert to string representation
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_operation_type_from_str() {
        use std::str::FromStr;
        assert_eq!(OperationType::from_str("add"), Ok(OperationType::Add));
        assert_eq!(OperationType::from_str("ADD"), Ok(OperationType::Add));
        assert_eq!(OperationType::from_str("update"), Ok(OperationType::Update));
        assert_eq!(OperationType::from_str("UPDATE"), Ok(OperationType::Update));
        assert_eq!(OperationType::from_str("delete"), Ok(OperationType::Delete));
        assert_eq!(OperationType::from_str("DELETE"), Ok(OperationType::Delete));
        assert!(OperationType::from_str("invalid").is_err());
    }

    #[test]
    fn test_operation_type_as_str() {
        assert_eq!(OperationType::Add.as_str(), "add");
        assert_eq!(OperationType::Update.as_str(), "update");
        assert_eq!(OperationType::Delete.as_str(), "delete");
    }

    #[test]
    fn test_template_spec_serialization() {
        let spec: TemplateSpec = TemplateSpec {
            template: "{{after.id}}".to_string(),
            extension: (),
        };

        let json = serde_json::to_string(&spec).unwrap();
        let deserialized: TemplateSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, deserialized);
    }

    #[test]
    fn test_query_config_serialization() {
        let config: QueryConfig = QueryConfig {
            added: Some(TemplateSpec {
                template: "[ADD] {{after.id}}".to_string(),
                extension: (),
            }),
            updated: None,
            deleted: Some(TemplateSpec {
                template: "[DEL] {{before.id}}".to_string(),
                extension: (),
            }),
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: QueryConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    #[derive(Debug, Clone)]
    struct TestReactionConfig {
        routes: HashMap<String, QueryConfig>,
        default_template: Option<QueryConfig>,
    }

    impl TemplateRouting for TestReactionConfig {
        fn routes(&self) -> &HashMap<String, QueryConfig> {
            &self.routes
        }

        fn default_template(&self) -> Option<&QueryConfig> {
            self.default_template.as_ref()
        }
    }

    #[test]
    fn test_template_routing_query_specific() {
        let mut routes = HashMap::new();
        routes.insert(
            "query1".to_string(),
            QueryConfig {
                added: Some(TemplateSpec {
                    template: "query1 add".to_string(),
                    extension: (),
                }),
                updated: None,
                deleted: None,
            },
        );

        let config = TestReactionConfig {
            routes,
            default_template: None,
        };

        let spec = config.get_template_spec("query1", OperationType::Add);
        assert!(spec.is_some());
        assert_eq!(spec.unwrap().template, "query1 add");

        let spec = config.get_template_spec("query1", OperationType::Update);
        assert!(spec.is_none());
    }

    #[test]
    fn test_template_routing_default_fallback() {
        let config = TestReactionConfig {
            routes: HashMap::new(),
            default_template: Some(QueryConfig {
                added: Some(TemplateSpec {
                    template: "default add".to_string(),
                    extension: (),
                }),
                updated: Some(TemplateSpec {
                    template: "default update".to_string(),
                    extension: (),
                }),
                deleted: Some(TemplateSpec {
                    template: "default delete".to_string(),
                    extension: (),
                }),
            }),
        };

        let spec = config.get_template_spec("any_query", OperationType::Add);
        assert!(spec.is_some());
        assert_eq!(spec.unwrap().template, "default add");

        let spec = config.get_template_spec("any_query", OperationType::Update);
        assert!(spec.is_some());
        assert_eq!(spec.unwrap().template, "default update");
    }

    #[test]
    fn test_template_routing_query_overrides_default() {
        let mut routes = HashMap::new();
        routes.insert(
            "query1".to_string(),
            QueryConfig {
                added: Some(TemplateSpec {
                    template: "query1 add override".to_string(),
                    extension: (),
                }),
                updated: None,
                deleted: None,
            },
        );

        let config = TestReactionConfig {
            routes,
            default_template: Some(QueryConfig {
                added: Some(TemplateSpec {
                    template: "default add".to_string(),
                    extension: (),
                }),
                updated: Some(TemplateSpec {
                    template: "default update".to_string(),
                    extension: (),
                }),
                deleted: None,
            }),
        };

        // Query-specific should override default
        let spec = config.get_template_spec("query1", OperationType::Add);
        assert_eq!(spec.unwrap().template, "query1 add override");

        // Missing query-specific should fall back to default
        let spec = config.get_template_spec("query1", OperationType::Update);
        assert_eq!(spec.unwrap().template, "default update");

        // Unknown query should use default
        let spec = config.get_template_spec("query2", OperationType::Add);
        assert_eq!(spec.unwrap().template, "default add");
    }
}
