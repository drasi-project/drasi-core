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

//! Content parsing for webhook payloads.
//!
//! Supports JSON, XML, YAML, and plain text content types with
//! automatic detection from Content-Type header.

use anyhow::{anyhow, Result};
use serde_json::Value as JsonValue;

/// Supported content types for webhook payloads
#[derive(Debug, Clone, PartialEq)]
pub enum ContentType {
    Json,
    Xml,
    Yaml,
    Text,
}

impl ContentType {
    /// Parse content type from Content-Type header value
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
                } else if lower.contains("text/plain") {
                    ContentType::Text
                } else {
                    // Default to JSON for unknown types
                    ContentType::Json
                }
            }
            None => ContentType::Json,
        }
    }
}

/// Parse content body into a JSON value based on content type
///
/// All content types are normalized to `serde_json::Value` for uniform
/// template processing.
pub fn parse_content(body: &[u8], content_type: ContentType) -> Result<JsonValue> {
    match content_type {
        ContentType::Json => parse_json(body),
        ContentType::Xml => parse_xml(body),
        ContentType::Yaml => parse_yaml(body),
        ContentType::Text => parse_text(body),
    }
}

/// Parse JSON content
fn parse_json(body: &[u8]) -> Result<JsonValue> {
    serde_json::from_slice(body).map_err(|e| anyhow!("Failed to parse JSON: {e}"))
}

/// Parse YAML content and convert to JSON value
fn parse_yaml(body: &[u8]) -> Result<JsonValue> {
    let yaml_value: serde_yaml::Value =
        serde_yaml::from_slice(body).map_err(|e| anyhow!("Failed to parse YAML: {e}"))?;
    yaml_to_json(yaml_value)
}

/// Convert YAML value to JSON value
fn yaml_to_json(yaml: serde_yaml::Value) -> Result<JsonValue> {
    match yaml {
        serde_yaml::Value::Null => Ok(JsonValue::Null),
        serde_yaml::Value::Bool(b) => Ok(JsonValue::Bool(b)),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(JsonValue::Number(i.into()))
            } else if let Some(f) = n.as_f64() {
                Ok(serde_json::Number::from_f64(f)
                    .map(JsonValue::Number)
                    .unwrap_or(JsonValue::Null))
            } else {
                Ok(JsonValue::Null)
            }
        }
        serde_yaml::Value::String(s) => Ok(JsonValue::String(s)),
        serde_yaml::Value::Sequence(seq) => {
            let arr: Result<Vec<JsonValue>> = seq.into_iter().map(yaml_to_json).collect();
            Ok(JsonValue::Array(arr?))
        }
        serde_yaml::Value::Mapping(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                let key = match k {
                    serde_yaml::Value::String(s) => s,
                    serde_yaml::Value::Number(n) => n.to_string(),
                    serde_yaml::Value::Bool(b) => b.to_string(),
                    _ => continue,
                };
                obj.insert(key, yaml_to_json(v)?);
            }
            Ok(JsonValue::Object(obj))
        }
        serde_yaml::Value::Tagged(tagged) => yaml_to_json(tagged.value),
    }
}

/// Parse XML content and convert to JSON value
fn parse_xml(body: &[u8]) -> Result<JsonValue> {
    let xml_str = std::str::from_utf8(body).map_err(|e| anyhow!("Invalid UTF-8 in XML: {e}"))?;
    xml_to_json(xml_str)
}

/// Convert XML string to JSON value
///
/// Uses a simplified conversion where:
/// - Elements become objects
/// - Text content goes into a "_text" field
/// - Attributes go into "@attribute_name" fields
/// - Repeated elements become arrays
fn xml_to_json(xml: &str) -> Result<JsonValue> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut stack: Vec<(String, JsonValue)> = vec![(
        "root".to_string(),
        JsonValue::Object(serde_json::Map::new()),
    )];

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let mut obj = serde_json::Map::new();

                // Add attributes
                for attr in e.attributes().flatten() {
                    let key = format!("@{}", String::from_utf8_lossy(attr.key.as_ref()));
                    let value = String::from_utf8_lossy(&attr.value).to_string();
                    obj.insert(key, JsonValue::String(value));
                }

                stack.push((name, JsonValue::Object(obj)));
            }
            Ok(Event::End(_)) => {
                if stack.len() > 1 {
                    if let Some((name, value)) = stack.pop() {
                        if let Some((_, JsonValue::Object(parent_obj))) = stack.last_mut() {
                            // Handle repeated elements by converting to array
                            if let Some(existing) = parent_obj.get_mut(&name) {
                                match existing {
                                    JsonValue::Array(arr) => arr.push(value),
                                    _ => {
                                        let prev = existing.take();
                                        *existing = JsonValue::Array(vec![prev, value]);
                                    }
                                }
                            } else {
                                parent_obj.insert(name, value);
                            }
                        }
                    }
                }
            }
            Ok(Event::Text(e)) => {
                let text = e
                    .unescape()
                    .map_err(|e| anyhow!("XML unescape error: {e}"))?;
                let text = text.trim();
                if !text.is_empty() {
                    if let Some((_, current)) = stack.last_mut() {
                        if let JsonValue::Object(obj) = current {
                            if obj.is_empty() {
                                // If no attributes, just use the text value directly
                                *current = JsonValue::String(text.to_string());
                            } else {
                                obj.insert(
                                    "_text".to_string(),
                                    JsonValue::String(text.to_string()),
                                );
                            }
                        }
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let mut obj = serde_json::Map::new();

                for attr in e.attributes().flatten() {
                    let key = format!("@{}", String::from_utf8_lossy(attr.key.as_ref()));
                    let value = String::from_utf8_lossy(&attr.value).to_string();
                    obj.insert(key, JsonValue::String(value));
                }

                let value = if obj.is_empty() {
                    JsonValue::Null
                } else {
                    JsonValue::Object(obj)
                };

                if let Some((_, JsonValue::Object(parent_obj))) = stack.last_mut() {
                    if let Some(existing) = parent_obj.get_mut(&name) {
                        match existing {
                            JsonValue::Array(arr) => arr.push(value),
                            _ => {
                                let prev = existing.take();
                                *existing = JsonValue::Array(vec![prev, value]);
                            }
                        }
                    } else {
                        parent_obj.insert(name, value);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow!("XML parse error: {e}")),
            _ => {}
        }
    }

    // Return the root object's content
    let Some((_, JsonValue::Object(mut root))) = stack.pop() else {
        return Err(anyhow!("Failed to parse XML structure"));
    };

    // If there's only one child, return it directly
    if root.len() == 1 {
        Ok(root
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .unwrap_or(JsonValue::Null))
    } else {
        Ok(JsonValue::Object(root))
    }
}

/// Parse plain text content
fn parse_text(body: &[u8]) -> Result<JsonValue> {
    let text =
        std::str::from_utf8(body).map_err(|e| anyhow!("Invalid UTF-8 in text content: {e}"))?;
    Ok(JsonValue::String(text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_type_from_header() {
        assert_eq!(
            ContentType::from_header(Some("application/json")),
            ContentType::Json
        );
        assert_eq!(
            ContentType::from_header(Some("application/json; charset=utf-8")),
            ContentType::Json
        );
        assert_eq!(
            ContentType::from_header(Some("text/json")),
            ContentType::Json
        );
        assert_eq!(
            ContentType::from_header(Some("application/xml")),
            ContentType::Xml
        );
        assert_eq!(ContentType::from_header(Some("text/xml")), ContentType::Xml);
        assert_eq!(
            ContentType::from_header(Some("application/x-yaml")),
            ContentType::Yaml
        );
        assert_eq!(
            ContentType::from_header(Some("text/yaml")),
            ContentType::Yaml
        );
        assert_eq!(
            ContentType::from_header(Some("text/plain")),
            ContentType::Text
        );
        assert_eq!(ContentType::from_header(None), ContentType::Json);
        assert_eq!(
            ContentType::from_header(Some("unknown/type")),
            ContentType::Json
        );
    }

    #[test]
    fn test_parse_json() {
        let json = r#"{"name": "test", "value": 42, "nested": {"key": "value"}}"#;
        let result = parse_content(json.as_bytes(), ContentType::Json).unwrap();

        assert_eq!(result["name"], "test");
        assert_eq!(result["value"], 42);
        assert_eq!(result["nested"]["key"], "value");
    }

    #[test]
    fn test_parse_json_array() {
        let json = r#"[1, 2, 3]"#;
        let result = parse_content(json.as_bytes(), ContentType::Json).unwrap();

        assert!(result.is_array());
        assert_eq!(result.as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_parse_yaml() {
        let yaml = r#"
name: test
value: 42
nested:
  key: value
items:
  - one
  - two
"#;
        let result = parse_content(yaml.as_bytes(), ContentType::Yaml).unwrap();

        assert_eq!(result["name"], "test");
        assert_eq!(result["value"], 42);
        assert_eq!(result["nested"]["key"], "value");
        assert_eq!(result["items"][0], "one");
        assert_eq!(result["items"][1], "two");
    }

    #[test]
    fn test_parse_xml_simple() {
        let xml = r#"<root><name>test</name><value>42</value></root>"#;
        let result = parse_content(xml.as_bytes(), ContentType::Xml).unwrap();

        assert_eq!(result["name"], "test");
        assert_eq!(result["value"], "42");
    }

    #[test]
    fn test_parse_xml_with_attributes() {
        let xml = r#"<item id="123" type="test">content</item>"#;
        let result = parse_content(xml.as_bytes(), ContentType::Xml).unwrap();

        assert_eq!(result["@id"], "123");
        assert_eq!(result["@type"], "test");
        assert_eq!(result["_text"], "content");
    }

    #[test]
    fn test_parse_xml_repeated_elements() {
        let xml = r#"<root><item>one</item><item>two</item><item>three</item></root>"#;
        let result = parse_content(xml.as_bytes(), ContentType::Xml).unwrap();

        assert!(result["item"].is_array());
        let items = result["item"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], "one");
        assert_eq!(items[1], "two");
        assert_eq!(items[2], "three");
    }

    #[test]
    fn test_parse_text() {
        let text = "Hello, World!";
        let result = parse_content(text.as_bytes(), ContentType::Text).unwrap();

        assert_eq!(result, JsonValue::String("Hello, World!".to_string()));
    }

    #[test]
    fn test_parse_invalid_json() {
        let invalid = "not valid json";
        let result = parse_content(invalid.as_bytes(), ContentType::Json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_invalid_yaml() {
        let invalid = "key: [unclosed";
        let result = parse_content(invalid.as_bytes(), ContentType::Yaml);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_invalid_xml() {
        // This is malformed XML with mismatched tags
        let invalid = "<root><unclosed></root>";
        let result = parse_content(invalid.as_bytes(), ContentType::Xml);
        assert!(result.is_err());
    }
}
