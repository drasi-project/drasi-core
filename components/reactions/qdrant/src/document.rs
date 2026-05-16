// Copyright 2026 The Drasi Authors.
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

//! Document processing utilities for the Qdrant reaction.
//!
//! This module provides:
//! - Deterministic UUID generation using RFC 4122 UUID v5
//! - Document building from query results using Handlebars templates
//! - Key extraction from nested JSON structures

use anyhow::{Context, Result};
use handlebars::Handlebars;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Namespace UUID for Drasi Qdrant point ID generation.
/// This is a fixed UUID used with UUID v5 to ensure deterministic generation.
const DRASI_QDRANT_NAMESPACE: Uuid = Uuid::from_bytes([
    0x64, 0x72, 0x61, 0x73, 0x69, 0x2d, 0x71, 0x64, // "drasi-qd"
    0x72, 0x61, 0x6e, 0x74, 0x2d, 0x6e, 0x73, 0x00, // "rant-ns\0"
]);

/// A document prepared for vector storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    /// Deterministic point ID generated from the key
    pub point_id: Uuid,

    /// Content text for embedding generation
    pub content: String,

    /// Optional document title
    pub title: Option<String>,

    /// Full metadata payload for the Qdrant point
    pub metadata: serde_json::Value,

    /// The original key used to generate the point ID
    pub key: String,
}

/// Builder for creating documents from query results.
pub struct DocumentBuilder<'a> {
    handlebars: &'a Handlebars<'static>,
    document_template_name: &'a str,
    title_template_name: Option<&'a str>,
    key_field: &'a str,
}

impl<'a> DocumentBuilder<'a> {
    /// Create a new document builder.
    ///
    /// # Arguments
    ///
    /// * `handlebars` - Handlebars instance with registered templates
    /// * `document_template_name` - Name of the registered document template
    /// * `title_template_name` - Optional name of the registered title template
    /// * `key_field` - Field path to extract the unique key (e.g., "id", "after.id")
    pub fn new(
        handlebars: &'a Handlebars<'static>,
        document_template_name: &'a str,
        title_template_name: Option<&'a str>,
        key_field: &'a str,
    ) -> Self {
        Self {
            handlebars,
            document_template_name,
            title_template_name,
            key_field,
        }
    }

    /// Build a document from query result data.
    ///
    /// # Arguments
    ///
    /// * `data` - JSON data from the query result
    ///
    /// # Returns
    ///
    /// A Document with generated content, title, point ID, and metadata.
    pub fn build(&self, data: &serde_json::Value) -> Result<Document> {
        // Extract key
        let key = extract_field(data, self.key_field)
            .with_context(|| format!("Failed to extract key field '{}'", self.key_field))?;

        // Generate deterministic UUID
        let point_id = generate_deterministic_uuid(&key);

        // Render document content
        let content = self
            .handlebars
            .render(self.document_template_name, data)
            .with_context(|| "Failed to render document template")?;

        // Render title if template is provided
        let title = if let Some(title_template) = self.title_template_name {
            Some(
                self.handlebars
                    .render(title_template, data)
                    .with_context(|| "Failed to render title template")?,
            )
        } else {
            None
        };

        Ok(Document {
            point_id,
            content,
            title,
            metadata: data.clone(),
            key,
        })
    }
}

/// Generate a deterministic UUID from a key string.
///
/// Uses UUID v5 (RFC 4122) with a Drasi-specific namespace. The same key
/// always produces the same UUID, enabling consistent point IDs for
/// upsert and delete operations.
///
/// # Example
///
/// ```
/// use drasi_reaction_qdrant::generate_deterministic_uuid;
///
/// let uuid1 = generate_deterministic_uuid("product-123");
/// let uuid2 = generate_deterministic_uuid("product-123");
/// assert_eq!(uuid1, uuid2);
///
/// let uuid3 = generate_deterministic_uuid("product-456");
/// assert_ne!(uuid1, uuid3);
/// ```
pub fn generate_deterministic_uuid(key: &str) -> Uuid {
    Uuid::new_v5(&DRASI_QDRANT_NAMESPACE, key.as_bytes())
}

/// Extract a field value from JSON data using a dot-separated path.
///
/// # Arguments
///
/// * `data` - JSON value to extract from
/// * `field_path` - Dot-separated path (e.g., "id", "after.product_id", "nested.deep.value")
///
/// # Returns
///
/// The string representation of the extracted value, or an error if not found.
pub fn extract_field(data: &serde_json::Value, field_path: &str) -> Result<String> {
    let parts: Vec<&str> = field_path.split('.').collect();
    let mut current = data;

    for part in &parts {
        current = current
            .get(*part)
            .with_context(|| format!("Field '{part}' not found in path '{field_path}'"))?;
    }

    match current {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        serde_json::Value::Bool(b) => Ok(b.to_string()),
        serde_json::Value::Null => Ok("null".to_string()),
        _ => Ok(current.to_string()),
    }
}

/// Setup Handlebars with document and title templates.
///
/// # Arguments
///
/// * `document_template` - Handlebars template for document content
/// * `title_template` - Optional Handlebars template for document title
///
/// # Returns
///
/// Configured Handlebars instance with templates registered.
pub fn setup_handlebars(
    document_template: &str,
    title_template: Option<&str>,
) -> Result<Handlebars<'static>> {
    let mut handlebars = Handlebars::new();

    // Disable HTML escaping for plain text content
    handlebars.register_escape_fn(handlebars::no_escape);

    handlebars
        .register_template_string("document", document_template)
        .with_context(|| "Failed to register document template")?;

    if let Some(title_tmpl) = title_template {
        handlebars
            .register_template_string("title", title_tmpl)
            .with_context(|| "Failed to register title template")?;
    }

    Ok(handlebars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_deterministic_uuid_consistency() {
        let key = "product-123";
        let uuid1 = generate_deterministic_uuid(key);
        let uuid2 = generate_deterministic_uuid(key);
        assert_eq!(uuid1, uuid2, "Same key should produce same UUID");
    }

    #[test]
    fn test_deterministic_uuid_different_keys() {
        let uuid1 = generate_deterministic_uuid("product-123");
        let uuid2 = generate_deterministic_uuid("product-456");
        assert_ne!(
            uuid1, uuid2,
            "Different keys should produce different UUIDs"
        );
    }

    #[test]
    fn test_deterministic_uuid_format() {
        let uuid = generate_deterministic_uuid("test-key");
        // Should be a valid UUID
        let uuid_str = uuid.to_string();
        assert_eq!(uuid_str.len(), 36); // UUID format: 8-4-4-4-12
        assert!(uuid_str.chars().filter(|c| *c == '-').count() == 4);
    }

    #[test]
    fn test_extract_field_simple() {
        let data = json!({
            "id": "123",
            "name": "Test Product"
        });

        let id = extract_field(&data, "id").expect("Failed to extract field");
        assert_eq!(id, "123");

        let name = extract_field(&data, "name").expect("Failed to extract field");
        assert_eq!(name, "Test Product");
    }

    #[test]
    fn test_extract_field_nested() {
        let data = json!({
            "after": {
                "product_id": "456",
                "details": {
                    "sku": "SKU-001"
                }
            }
        });

        let id = extract_field(&data, "after.product_id").expect("Failed to extract field");
        assert_eq!(id, "456");

        let sku = extract_field(&data, "after.details.sku").expect("Failed to extract field");
        assert_eq!(sku, "SKU-001");
    }

    #[test]
    fn test_extract_field_numeric() {
        let data = json!({
            "count": 42,
            "price": 19.99
        });

        let count = extract_field(&data, "count").expect("Failed to extract field");
        assert_eq!(count, "42");

        let price = extract_field(&data, "price").expect("Failed to extract field");
        assert_eq!(price, "19.99");
    }

    #[test]
    fn test_extract_field_boolean() {
        let data = json!({
            "active": true
        });

        let active = extract_field(&data, "active").expect("Failed to extract field");
        assert_eq!(active, "true");
    }

    #[test]
    fn test_extract_field_not_found() {
        let data = json!({
            "id": "123"
        });

        let result = extract_field(&data, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_document_builder() {
        let handlebars = setup_handlebars(
            "Product: {{name}}. Description: {{description}}",
            Some("{{name}}"),
        )
        .expect("Failed to setup handlebars");

        let builder = DocumentBuilder::new(&handlebars, "document", Some("title"), "id");

        let data = json!({
            "id": "prod-001",
            "name": "Widget",
            "description": "A useful widget"
        });

        let doc = builder.build(&data).expect("Failed to build document");

        assert_eq!(doc.key, "prod-001");
        assert_eq!(doc.content, "Product: Widget. Description: A useful widget");
        assert_eq!(doc.title, Some("Widget".to_string()));
        assert_eq!(doc.point_id, generate_deterministic_uuid("prod-001"));
    }

    #[test]
    fn test_document_builder_no_title() {
        let handlebars =
            setup_handlebars("Content: {{content}}", None).expect("Failed to setup handlebars");

        let builder = DocumentBuilder::new(&handlebars, "document", None, "id");

        let data = json!({
            "id": "item-001",
            "content": "Some content here"
        });

        let doc = builder.build(&data).expect("Failed to build document");

        assert_eq!(doc.key, "item-001");
        assert_eq!(doc.content, "Content: Some content here");
        assert!(doc.title.is_none());
    }

    #[test]
    fn test_document_builder_nested_key() {
        let handlebars =
            setup_handlebars("{{after.name}}", None).expect("Failed to setup handlebars");

        let builder = DocumentBuilder::new(&handlebars, "document", None, "after.id");

        let data = json!({
            "after": {
                "id": "nested-123",
                "name": "Nested Item"
            }
        });

        let doc = builder.build(&data).expect("Failed to build document");

        assert_eq!(doc.key, "nested-123");
        assert_eq!(doc.content, "Nested Item");
    }

    #[test]
    fn test_handlebars_no_escape() {
        let handlebars = setup_handlebars("{{content}}", None).expect("Failed to setup handlebars");

        let builder = DocumentBuilder::new(&handlebars, "document", None, "id");

        let data = json!({
            "id": "html-001",
            "content": "<p>HTML & special \"chars\"</p>"
        });

        let doc = builder.build(&data).expect("Failed to build document");

        // Should not escape HTML
        assert_eq!(doc.content, "<p>HTML & special \"chars\"</p>");
    }
}
