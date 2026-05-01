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

//! Content parsing for HTTP bootstrap responses.
//!
//! Supports JSON, XML, YAML content types with automatic detection
//! from Content-Type header.

use anyhow::{anyhow, Context, Result};
use serde_json::Value as JsonValue;

use crate::config::ContentTypeOverride;

/// Detected content type.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentType {
    Json,
    Xml,
    Yaml,
}

impl ContentType {
    /// Detect content type from Content-Type header value.
    pub fn from_header(header: Option<&str>) -> Self {
        match header {
            Some(h) => {
                let lower = h.to_lowercase();
                if lower.contains("application/json") || lower.contains("text/json") {
                    ContentType::Json
                } else if lower.contains("application/xml") || lower.contains("text/xml") {
                    ContentType::Xml
                } else if lower.contains("application/x-yaml")
                    || lower.contains("text/yaml")
                    || lower.contains("application/yaml")
                {
                    ContentType::Yaml
                } else {
                    // Default to JSON for unknown types
                    ContentType::Json
                }
            }
            None => ContentType::Json,
        }
    }

    /// Create from override configuration.
    pub fn from_override(override_type: &ContentTypeOverride) -> Self {
        match override_type {
            ContentTypeOverride::Json => ContentType::Json,
            ContentTypeOverride::Xml => ContentType::Xml,
            ContentTypeOverride::Yaml => ContentType::Yaml,
        }
    }
}

/// Parse response body into a JSON value based on content type.
pub fn parse_body(body: &str, content_type: &ContentType) -> Result<JsonValue> {
    match content_type {
        ContentType::Json => {
            serde_json::from_str(body).context("Failed to parse response body as JSON")
        }
        ContentType::Xml => parse_xml(body),
        ContentType::Yaml => {
            serde_yaml::from_str(body).context("Failed to parse response body as YAML")
        }
    }
}

/// Parse XML body into a JSON value.
fn parse_xml(body: &str) -> Result<JsonValue> {
    // Use quick-xml to parse into a simple JSON structure
    let value: JsonValue =
        quick_xml::de::from_str(body).context("Failed to parse response body as XML")?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_type_detection() {
        assert_eq!(
            ContentType::from_header(Some("application/json")),
            ContentType::Json
        );
        assert_eq!(
            ContentType::from_header(Some("application/json; charset=utf-8")),
            ContentType::Json
        );
        assert_eq!(
            ContentType::from_header(Some("application/xml")),
            ContentType::Xml
        );
        assert_eq!(
            ContentType::from_header(Some("text/yaml")),
            ContentType::Yaml
        );
        assert_eq!(ContentType::from_header(None), ContentType::Json);
        assert_eq!(
            ContentType::from_header(Some("text/plain")),
            ContentType::Json
        );
    }

    #[test]
    fn test_parse_json() {
        let body = r#"{"name": "test", "value": 42}"#;
        let result = parse_body(body, &ContentType::Json).unwrap();
        assert_eq!(result["name"], "test");
        assert_eq!(result["value"], 42);
    }

    #[test]
    fn test_parse_yaml() {
        let body = "name: test\nvalue: 42\n";
        let result = parse_body(body, &ContentType::Yaml).unwrap();
        assert_eq!(result["name"], "test");
        assert_eq!(result["value"], 42);
    }

    #[test]
    fn test_parse_xml_simple() {
        let body = "<root><name>test</name><value>42</value></root>";
        let result = parse_body(body, &ContentType::Xml).unwrap();
        assert!(result.get("name").is_some());
        assert!(result.get("value").is_some());
    }

    #[test]
    fn test_parse_xml_with_text_fields() {
        // quick-xml wraps text content in $text for nested elements
        let body = "<root><item><id>x1</id><name>Alice</name></item></root>";
        let result = parse_body(body, &ContentType::Xml).unwrap();
        let item = &result["item"];
        // quick-xml produces {"$text": "x1"} for <id>x1</id>
        assert!(item["id"].is_object() || item["id"].is_string());
    }
}
