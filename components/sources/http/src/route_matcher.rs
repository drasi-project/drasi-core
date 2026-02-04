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

//! Route matching and condition evaluation for webhooks.
//!
//! Handles:
//! - Route pattern matching with path parameters
//! - Mapping condition evaluation (header/field matching)

use crate::config::{HttpMethod, MappingCondition, WebhookMapping, WebhookRoute};
use regex::Regex;
use serde_json::Value as JsonValue;
use std::collections::HashMap;

/// Result of route matching
#[derive(Debug, Clone)]
pub struct RouteMatch<'a> {
    /// The matched route configuration
    pub route: &'a WebhookRoute,
    /// Extracted path parameters
    pub path_params: HashMap<String, String>,
}

/// Route matcher for webhook routes
pub struct RouteMatcher {
    /// Compiled route patterns
    routes: Vec<CompiledRoute>,
}

/// A compiled route pattern
struct CompiledRoute {
    /// Original route index
    index: usize,
    /// Compiled regex for matching
    pattern: Regex,
    /// Names of path parameters in order
    param_names: Vec<String>,
    /// Allowed HTTP methods
    methods: Vec<HttpMethod>,
}

impl RouteMatcher {
    /// Create a new route matcher from webhook routes
    pub fn new(routes: &[WebhookRoute]) -> Self {
        let compiled: Vec<CompiledRoute> = routes
            .iter()
            .enumerate()
            .filter_map(|(idx, route)| compile_route(idx, route))
            .collect();

        Self { routes: compiled }
    }

    /// Match an incoming request against configured routes
    pub fn match_route<'a>(
        &self,
        path: &str,
        method: &HttpMethod,
        routes: &'a [WebhookRoute],
    ) -> Option<RouteMatch<'a>> {
        for compiled in &self.routes {
            // Check method first (faster)
            if !compiled.methods.contains(method) {
                continue;
            }

            // Try to match the path
            if let Some(captures) = compiled.pattern.captures(path) {
                let mut path_params = HashMap::new();

                // Extract named parameters
                for (i, name) in compiled.param_names.iter().enumerate() {
                    if let Some(value) = captures.get(i + 1) {
                        path_params.insert(name.clone(), value.as_str().to_string());
                    }
                }

                return Some(RouteMatch {
                    route: &routes[compiled.index],
                    path_params,
                });
            }
        }

        None
    }
}

/// Compile a route pattern into a regex
fn compile_route(index: usize, route: &WebhookRoute) -> Option<CompiledRoute> {
    let mut pattern = String::from("^");
    let mut param_names = Vec::new();

    for segment in route.path.split('/') {
        if segment.is_empty() {
            continue;
        }

        pattern.push('/');

        if let Some(param) = segment.strip_prefix(':') {
            // Path parameter
            param_names.push(param.to_string());
            pattern.push_str("([^/]+)");
        } else if let Some(param) = segment.strip_prefix('*') {
            // Wildcard parameter (matches multiple segments)
            param_names.push(param.to_string());
            pattern.push_str("(.+)");
        } else {
            // Literal segment - escape regex special chars
            pattern.push_str(&regex::escape(segment));
        }
    }

    pattern.push('$');

    match Regex::new(&pattern) {
        Ok(regex) => Some(CompiledRoute {
            index,
            pattern: regex,
            param_names,
            methods: route.methods.clone(),
        }),
        Err(e) => {
            log::error!("Failed to compile route pattern '{}': {}", route.path, e);
            None
        }
    }
}

/// Evaluate mapping conditions to find matching mappings
pub fn find_matching_mappings<'a>(
    mappings: &'a [WebhookMapping],
    headers: &HashMap<String, String>,
    payload: &JsonValue,
) -> Vec<&'a WebhookMapping> {
    mappings
        .iter()
        .filter(|m| evaluate_condition(m.when.as_ref(), headers, payload))
        .collect()
}

/// Evaluate a single mapping condition
fn evaluate_condition(
    condition: Option<&MappingCondition>,
    headers: &HashMap<String, String>,
    payload: &JsonValue,
) -> bool {
    let Some(cond) = condition else {
        // No condition means always match
        return true;
    };

    // Get the value to check
    let value = if let Some(ref header_name) = cond.header {
        // Case-insensitive header lookup
        let lower_name = header_name.to_lowercase();
        headers
            .iter()
            .find(|(k, _)| k.to_lowercase() == lower_name)
            .map(|(_, v)| v.as_str())
    } else if let Some(ref field_path) = cond.field {
        // Get value from payload
        resolve_json_path(payload, field_path).and_then(|v| v.as_str())
    } else {
        // No source specified
        return false;
    };

    let Some(value_str) = value else {
        // Value not found
        return false;
    };

    // Check conditions
    if let Some(ref expected) = cond.equals {
        if value_str != expected {
            return false;
        }
    }

    if let Some(ref substring) = cond.contains {
        if !value_str.contains(substring) {
            return false;
        }
    }

    if let Some(ref regex_pattern) = cond.regex {
        match Regex::new(regex_pattern) {
            Ok(re) => {
                if !re.is_match(value_str) {
                    return false;
                }
            }
            Err(e) => {
                log::warn!("Invalid regex pattern '{regex_pattern}': {e}");
                return false;
            }
        }
    }

    true
}

/// Resolve a dot-separated path in a JSON value
fn resolve_json_path<'a>(value: &'a JsonValue, path: &str) -> Option<&'a JsonValue> {
    // Handle "payload." prefix
    let path = path.strip_prefix("payload.").unwrap_or(path);

    let mut current = value;
    for part in path.split('.') {
        current = match current {
            JsonValue::Object(obj) => obj.get(part)?,
            JsonValue::Array(arr) => {
                let index: usize = part.parse().ok()?;
                arr.get(index)?
            }
            _ => return None,
        };
    }
    Some(current)
}

/// Convert Axum method to our HttpMethod enum
pub fn convert_method(method: &axum::http::Method) -> Option<HttpMethod> {
    match *method {
        axum::http::Method::GET => Some(HttpMethod::Get),
        axum::http::Method::POST => Some(HttpMethod::Post),
        axum::http::Method::PUT => Some(HttpMethod::Put),
        axum::http::Method::PATCH => Some(HttpMethod::Patch),
        axum::http::Method::DELETE => Some(HttpMethod::Delete),
        _ => None,
    }
}

/// Convert Axum headers to HashMap
pub fn headers_to_map(headers: &axum::http::HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.to_string(), v.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ElementTemplate, ElementType, OperationType};

    fn create_test_route(path: &str, methods: Vec<HttpMethod>) -> WebhookRoute {
        WebhookRoute {
            path: path.to_string(),
            methods,
            auth: None,
            error_behavior: None,
            mappings: vec![WebhookMapping {
                when: None,
                operation: Some(OperationType::Insert),
                operation_from: None,
                operation_map: None,
                element_type: ElementType::Node,
                effective_from: None,
                template: ElementTemplate {
                    id: "test".to_string(),
                    labels: vec!["Test".to_string()],
                    properties: None,
                    from: None,
                    to: None,
                },
            }],
        }
    }

    #[test]
    fn test_simple_route_matching() {
        let routes = vec![
            create_test_route("/webhooks/github", vec![HttpMethod::Post]),
            create_test_route("/webhooks/shopify", vec![HttpMethod::Post]),
        ];

        let matcher = RouteMatcher::new(&routes);

        let result = matcher.match_route("/webhooks/github", &HttpMethod::Post, &routes);
        assert!(result.is_some());
        assert_eq!(result.unwrap().route.path, "/webhooks/github");

        let result = matcher.match_route("/webhooks/shopify", &HttpMethod::Post, &routes);
        assert!(result.is_some());
        assert_eq!(result.unwrap().route.path, "/webhooks/shopify");

        let result = matcher.match_route("/webhooks/unknown", &HttpMethod::Post, &routes);
        assert!(result.is_none());
    }

    #[test]
    fn test_route_with_path_params() {
        let routes = vec![create_test_route(
            "/users/:user_id/events/:event_id",
            vec![HttpMethod::Post],
        )];

        let matcher = RouteMatcher::new(&routes);

        let result = matcher.match_route("/users/123/events/456", &HttpMethod::Post, &routes);
        assert!(result.is_some());

        let route_match = result.unwrap();
        assert_eq!(
            route_match.path_params.get("user_id"),
            Some(&"123".to_string())
        );
        assert_eq!(
            route_match.path_params.get("event_id"),
            Some(&"456".to_string())
        );
    }

    #[test]
    fn test_route_method_filtering() {
        let routes = vec![create_test_route(
            "/events",
            vec![HttpMethod::Post, HttpMethod::Put],
        )];

        let matcher = RouteMatcher::new(&routes);

        // POST should match
        let result = matcher.match_route("/events", &HttpMethod::Post, &routes);
        assert!(result.is_some());

        // PUT should match
        let result = matcher.match_route("/events", &HttpMethod::Put, &routes);
        assert!(result.is_some());

        // GET should not match
        let result = matcher.match_route("/events", &HttpMethod::Get, &routes);
        assert!(result.is_none());
    }

    #[test]
    fn test_condition_header_equals() {
        let condition = MappingCondition {
            header: Some("X-Event-Type".to_string()),
            field: None,
            equals: Some("push".to_string()),
            contains: None,
            regex: None,
        };

        let mut headers = HashMap::new();
        headers.insert("X-Event-Type".to_string(), "push".to_string());

        let payload = JsonValue::Null;

        assert!(evaluate_condition(Some(&condition), &headers, &payload));

        headers.insert("X-Event-Type".to_string(), "pull".to_string());
        assert!(!evaluate_condition(Some(&condition), &headers, &payload));
    }

    #[test]
    fn test_condition_header_case_insensitive() {
        let condition = MappingCondition {
            header: Some("x-event-type".to_string()),
            field: None,
            equals: Some("push".to_string()),
            contains: None,
            regex: None,
        };

        let mut headers = HashMap::new();
        headers.insert("X-Event-Type".to_string(), "push".to_string());

        let payload = JsonValue::Null;

        assert!(evaluate_condition(Some(&condition), &headers, &payload));
    }

    #[test]
    fn test_condition_field_equals() {
        let condition = MappingCondition {
            header: None,
            field: Some("payload.action".to_string()),
            equals: Some("created".to_string()),
            contains: None,
            regex: None,
        };

        let headers = HashMap::new();
        let payload = serde_json::json!({
            "action": "created"
        });

        assert!(evaluate_condition(Some(&condition), &headers, &payload));

        let payload = serde_json::json!({
            "action": "deleted"
        });
        assert!(!evaluate_condition(Some(&condition), &headers, &payload));
    }

    #[test]
    fn test_condition_contains() {
        let condition = MappingCondition {
            header: Some("User-Agent".to_string()),
            field: None,
            equals: None,
            contains: Some("GitHub".to_string()),
            regex: None,
        };

        let mut headers = HashMap::new();
        headers.insert(
            "User-Agent".to_string(),
            "GitHub-Hookshot/abc123".to_string(),
        );

        let payload = JsonValue::Null;

        assert!(evaluate_condition(Some(&condition), &headers, &payload));

        headers.insert("User-Agent".to_string(), "curl/7.0".to_string());
        assert!(!evaluate_condition(Some(&condition), &headers, &payload));
    }

    #[test]
    fn test_condition_regex() {
        let condition = MappingCondition {
            header: None,
            field: Some("payload.version".to_string()),
            equals: None,
            contains: None,
            regex: Some(r"^v\d+\.\d+\.\d+$".to_string()),
        };

        let headers = HashMap::new();

        let payload = serde_json::json!({
            "version": "v1.2.3"
        });
        assert!(evaluate_condition(Some(&condition), &headers, &payload));

        let payload = serde_json::json!({
            "version": "1.2.3"
        });
        assert!(!evaluate_condition(Some(&condition), &headers, &payload));
    }

    #[test]
    fn test_no_condition_always_matches() {
        let headers = HashMap::new();
        let payload = JsonValue::Null;

        assert!(evaluate_condition(None, &headers, &payload));
    }

    #[test]
    fn test_find_matching_mappings() {
        let mappings = vec![
            WebhookMapping {
                when: Some(MappingCondition {
                    header: Some("X-Event".to_string()),
                    field: None,
                    equals: Some("push".to_string()),
                    contains: None,
                    regex: None,
                }),
                operation: Some(OperationType::Insert),
                operation_from: None,
                operation_map: None,
                element_type: ElementType::Node,
                effective_from: None,
                template: ElementTemplate {
                    id: "push".to_string(),
                    labels: vec!["Push".to_string()],
                    properties: None,
                    from: None,
                    to: None,
                },
            },
            WebhookMapping {
                when: Some(MappingCondition {
                    header: Some("X-Event".to_string()),
                    field: None,
                    equals: Some("pull".to_string()),
                    contains: None,
                    regex: None,
                }),
                operation: Some(OperationType::Insert),
                operation_from: None,
                operation_map: None,
                element_type: ElementType::Node,
                effective_from: None,
                template: ElementTemplate {
                    id: "pull".to_string(),
                    labels: vec!["Pull".to_string()],
                    properties: None,
                    from: None,
                    to: None,
                },
            },
        ];

        let mut headers = HashMap::new();
        headers.insert("X-Event".to_string(), "push".to_string());
        let payload = JsonValue::Null;

        let matches = find_matching_mappings(&mappings, &headers, &payload);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].template.id, "push");

        headers.insert("X-Event".to_string(), "pull".to_string());
        let matches = find_matching_mappings(&mappings, &headers, &payload);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].template.id, "pull");
    }

    #[test]
    fn test_resolve_json_path() {
        let json = serde_json::json!({
            "user": {
                "name": "John",
                "address": {
                    "city": "NYC"
                }
            },
            "items": ["a", "b", "c"]
        });

        assert_eq!(
            resolve_json_path(&json, "user.name"),
            Some(&JsonValue::String("John".to_string()))
        );
        assert_eq!(
            resolve_json_path(&json, "user.address.city"),
            Some(&JsonValue::String("NYC".to_string()))
        );
        assert_eq!(
            resolve_json_path(&json, "items.0"),
            Some(&JsonValue::String("a".to_string()))
        );
        assert_eq!(resolve_json_path(&json, "missing"), None);
    }

    #[test]
    fn test_convert_method() {
        assert_eq!(
            convert_method(&axum::http::Method::GET),
            Some(HttpMethod::Get)
        );
        assert_eq!(
            convert_method(&axum::http::Method::POST),
            Some(HttpMethod::Post)
        );
        assert_eq!(
            convert_method(&axum::http::Method::PUT),
            Some(HttpMethod::Put)
        );
        assert_eq!(
            convert_method(&axum::http::Method::PATCH),
            Some(HttpMethod::Patch)
        );
        assert_eq!(
            convert_method(&axum::http::Method::DELETE),
            Some(HttpMethod::Delete)
        );
        assert_eq!(convert_method(&axum::http::Method::HEAD), None);
    }
}
