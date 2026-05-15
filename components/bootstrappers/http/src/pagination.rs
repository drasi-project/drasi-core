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

//! Pagination strategies for HTTP bootstrap requests.

use anyhow::{anyhow, Result};
use reqwest::header::HeaderMap;
use serde_json::Value as JsonValue;
use url::Url;

use crate::config::PaginationConfig;

/// Describes how to modify the next request for pagination.
#[derive(Debug)]
pub enum NextPage {
    /// Modify query parameters on the original URL.
    QueryParams(Vec<(String, String)>),
    /// Use a completely new URL for the next request.
    NewUrl(String),
}

/// Trait for pagination state machines.
pub trait Paginator: Send + Sync {
    /// Initialize the paginator, returning any query params for the first request.
    fn initial_params(&self) -> Vec<(String, String)>;

    /// Given the previous response body and headers, determine the next page request.
    /// Returns None if there are no more pages.
    fn next_page(
        &mut self,
        response_body: &JsonValue,
        response_headers: &HeaderMap,
        items_count: usize,
    ) -> Result<Option<NextPage>>;
}

/// Validate that a pagination-followed URL shares the same scheme+host as the
/// original endpoint URL (SSRF prevention).
fn validate_pagination_url(next_url: &str, origin_host: &str) -> Result<String> {
    let parsed = Url::parse(next_url)
        .map_err(|e| anyhow!("Invalid pagination URL '{}': {}", next_url, e))?;

    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(anyhow!(
            "Pagination URL has disallowed scheme '{}': {}",
            scheme,
            next_url
        ));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("Pagination URL has no host: {}", next_url))?;

    if host != origin_host {
        return Err(anyhow!(
            "Pagination URL host '{}' does not match origin host '{}' (SSRF protection)",
            host,
            origin_host
        ));
    }

    Ok(next_url.to_string())
}

/// Extract the host from a URL string for SSRF origin validation.
pub fn extract_origin_host(url: &str) -> Option<String> {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
}

/// Create a paginator from configuration.
pub fn create_paginator(config: &PaginationConfig, origin_host: String) -> Box<dyn Paginator> {
    match config {
        PaginationConfig::OffsetLimit {
            offset_param,
            limit_param,
            page_size,
            total_path,
        } => Box::new(OffsetLimitPaginator {
            offset_param: offset_param.clone(),
            limit_param: limit_param.clone(),
            page_size: *page_size,
            total_path: total_path.clone(),
            current_offset: 0,
        }),
        PaginationConfig::PageNumber {
            page_param,
            page_size_param,
            page_size,
            total_pages_path,
        } => Box::new(PageNumberPaginator {
            page_param: page_param.clone(),
            page_size_param: page_size_param.clone(),
            page_size: *page_size,
            total_pages_path: total_pages_path.clone(),
            current_page: 1,
        }),
        PaginationConfig::Cursor {
            cursor_param,
            cursor_path,
            has_more_path,
            page_size_param,
            page_size,
        } => Box::new(CursorPaginator {
            cursor_param: cursor_param.clone(),
            cursor_path: cursor_path.clone(),
            has_more_path: has_more_path.clone(),
            page_size_param: page_size_param.clone(),
            page_size: *page_size,
        }),
        PaginationConfig::LinkHeader {
            page_size_param,
            page_size,
        } => Box::new(LinkHeaderPaginator {
            page_size_param: page_size_param.clone(),
            page_size: *page_size,
            origin_host: origin_host.clone(),
        }),
        PaginationConfig::NextUrl {
            next_url_path,
            base_url,
        } => Box::new(NextUrlPaginator {
            next_url_path: next_url_path.clone(),
            base_url: base_url.clone(),
            origin_host,
        }),
    }
}

// ── Offset/Limit ────────────────────────────────────────────────────────────

struct OffsetLimitPaginator {
    offset_param: String,
    limit_param: String,
    page_size: u64,
    total_path: Option<String>,
    current_offset: u64,
}

impl Paginator for OffsetLimitPaginator {
    fn initial_params(&self) -> Vec<(String, String)> {
        vec![
            (self.offset_param.clone(), "0".to_string()),
            (self.limit_param.clone(), self.page_size.to_string()),
        ]
    }

    fn next_page(
        &mut self,
        response_body: &JsonValue,
        _response_headers: &HeaderMap,
        items_count: usize,
    ) -> Result<Option<NextPage>> {
        self.current_offset += self.page_size;

        // If we got fewer items than page_size, we're done
        if (items_count as u64) < self.page_size {
            return Ok(None);
        }

        // If total_path is set, check if we've fetched everything
        if let Some(ref total_path) = self.total_path {
            if let Some(total) = extract_json_path_u64(response_body, total_path) {
                if self.current_offset >= total {
                    return Ok(None);
                }
            }
        }

        Ok(Some(NextPage::QueryParams(vec![
            (self.offset_param.clone(), self.current_offset.to_string()),
            (self.limit_param.clone(), self.page_size.to_string()),
        ])))
    }
}

// ── Page Number ─────────────────────────────────────────────────────────────

struct PageNumberPaginator {
    page_param: String,
    page_size_param: String,
    page_size: u64,
    total_pages_path: Option<String>,
    current_page: u64,
}

impl Paginator for PageNumberPaginator {
    fn initial_params(&self) -> Vec<(String, String)> {
        vec![
            (self.page_param.clone(), "1".to_string()),
            (self.page_size_param.clone(), self.page_size.to_string()),
        ]
    }

    fn next_page(
        &mut self,
        response_body: &JsonValue,
        _response_headers: &HeaderMap,
        items_count: usize,
    ) -> Result<Option<NextPage>> {
        self.current_page += 1;

        // If we got fewer items than page_size, we're done
        if (items_count as u64) < self.page_size {
            return Ok(None);
        }

        // If total_pages_path is set, check if we've exceeded total pages
        if let Some(ref total_path) = self.total_pages_path {
            if let Some(total_pages) = extract_json_path_u64(response_body, total_path) {
                if self.current_page > total_pages {
                    return Ok(None);
                }
            }
        }

        Ok(Some(NextPage::QueryParams(vec![
            (self.page_param.clone(), self.current_page.to_string()),
            (self.page_size_param.clone(), self.page_size.to_string()),
        ])))
    }
}

// ── Cursor ──────────────────────────────────────────────────────────────────

struct CursorPaginator {
    cursor_param: String,
    cursor_path: String,
    has_more_path: Option<String>,
    page_size_param: Option<String>,
    page_size: Option<u64>,
}

impl Paginator for CursorPaginator {
    fn initial_params(&self) -> Vec<(String, String)> {
        let mut params = Vec::new();
        if let (Some(ref param), Some(size)) = (&self.page_size_param, self.page_size) {
            params.push((param.clone(), size.to_string()));
        }
        params
    }

    fn next_page(
        &mut self,
        response_body: &JsonValue,
        _response_headers: &HeaderMap,
        items_count: usize,
    ) -> Result<Option<NextPage>> {
        // If has_more_path is set, check it
        if let Some(ref has_more_path) = self.has_more_path {
            if let Some(has_more) = extract_json_path_bool(response_body, has_more_path) {
                if !has_more {
                    return Ok(None);
                }
            }
        }

        // If no items were returned, we're done
        if items_count == 0 {
            return Ok(None);
        }

        // Extract cursor value for next request
        let cursor = extract_json_path_string(response_body, &self.cursor_path);
        match cursor {
            Some(cursor_value) if !cursor_value.is_empty() => {
                let mut params = vec![(self.cursor_param.clone(), cursor_value)];
                if let (Some(ref param), Some(size)) = (&self.page_size_param, self.page_size) {
                    params.push((param.clone(), size.to_string()));
                }
                Ok(Some(NextPage::QueryParams(params)))
            }
            _ => Ok(None),
        }
    }
}

// ── Link Header ─────────────────────────────────────────────────────────────

struct LinkHeaderPaginator {
    page_size_param: Option<String>,
    page_size: Option<u64>,
    origin_host: String,
}

impl Paginator for LinkHeaderPaginator {
    fn initial_params(&self) -> Vec<(String, String)> {
        let mut params = Vec::new();
        if let (Some(ref param), Some(size)) = (&self.page_size_param, self.page_size) {
            params.push((param.clone(), size.to_string()));
        }
        params
    }

    fn next_page(
        &mut self,
        _response_body: &JsonValue,
        response_headers: &HeaderMap,
        items_count: usize,
    ) -> Result<Option<NextPage>> {
        if items_count == 0 {
            return Ok(None);
        }

        let next_url = parse_link_header_next(response_headers);
        match next_url {
            Some(url) => {
                let validated = validate_pagination_url(&url, &self.origin_host)?;
                Ok(Some(NextPage::NewUrl(validated)))
            }
            None => Ok(None),
        }
    }
}

// ── Next URL ────────────────────────────────────────────────────────────────

struct NextUrlPaginator {
    next_url_path: String,
    base_url: Option<String>,
    origin_host: String,
}

impl Paginator for NextUrlPaginator {
    fn initial_params(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn next_page(
        &mut self,
        response_body: &JsonValue,
        _response_headers: &HeaderMap,
        _items_count: usize,
    ) -> Result<Option<NextPage>> {
        let next_url = extract_json_path_string(response_body, &self.next_url_path);
        match next_url {
            Some(url) if !url.is_empty() => {
                // If it's a relative URL and we have a base_url, combine them
                let full_url = if url.starts_with("http://") || url.starts_with("https://") {
                    url
                } else if let Some(ref base) = self.base_url {
                    format!("{}{}", base.trim_end_matches('/'), url)
                } else {
                    url
                };
                let validated = validate_pagination_url(&full_url, &self.origin_host)?;
                Ok(Some(NextPage::NewUrl(validated)))
            }
            _ => Ok(None),
        }
    }
}

// ── Helper functions ────────────────────────────────────────────────────────

/// Extract a string value from a JSON document using a simple path expression.
/// Supports dot-notation paths like "$.data[-1].id" or "$.nextRecordsUrl".
pub fn extract_json_path_string(value: &JsonValue, path: &str) -> Option<String> {
    let result = navigate_path(value, path)?;
    match result {
        JsonValue::String(s) => Some(s.clone()),
        JsonValue::Number(n) => Some(n.to_string()),
        JsonValue::Bool(b) => Some(b.to_string()),
        JsonValue::Null => None,
        _ => Some(result.to_string()),
    }
}

/// Extract a u64 value from a JSON document using a path expression.
pub fn extract_json_path_u64(value: &JsonValue, path: &str) -> Option<u64> {
    let result = navigate_path(value, path)?;
    result.as_u64()
}

/// Extract a boolean value from a JSON document using a path expression.
pub fn extract_json_path_bool(value: &JsonValue, path: &str) -> Option<bool> {
    let result = navigate_path(value, path)?;
    result.as_bool()
}

/// Navigate a JSON value using a simple JSONPath-like expression.
/// Supports: $.field, $.field.nested, $.array[0], $.array[-1]
pub fn navigate_path<'a>(value: &'a JsonValue, path: &str) -> Option<&'a JsonValue> {
    let path = path
        .strip_prefix("$.")
        .unwrap_or(path.strip_prefix("$").unwrap_or(path));

    if path.is_empty() {
        return Some(value);
    }

    let mut current = value;
    for segment in split_path_segments(path) {
        current = navigate_segment(current, &segment)?;
    }
    Some(current)
}

/// Split a path into segments, handling bracket notation.
fn split_path_segments(path: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();

    let chars: Vec<char> = path.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '.' => {
                if !current.is_empty() {
                    segments.push(current.clone());
                    current.clear();
                }
            }
            '[' => {
                if !current.is_empty() {
                    segments.push(current.clone());
                    current.clear();
                }
                // Find closing bracket
                let mut bracket_content = String::new();
                i += 1;
                while i < chars.len() && chars[i] != ']' {
                    bracket_content.push(chars[i]);
                    i += 1;
                }
                segments.push(format!("[{bracket_content}]"));
            }
            c => {
                current.push(c);
            }
        }
        i += 1;
    }

    if !current.is_empty() {
        segments.push(current);
    }

    segments
}

/// Navigate a single path segment.
fn navigate_segment<'a>(value: &'a JsonValue, segment: &str) -> Option<&'a JsonValue> {
    if let Some(index_str) = segment.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        // Array index
        let arr = value.as_array()?;
        if arr.is_empty() {
            return None;
        }
        let index: i64 = index_str.parse().ok()?;
        let len = arr.len() as i64;
        let actual_index = if index < 0 {
            // Bounds check: ensure -index <= len
            if -index > len {
                return None;
            }
            (len + index) as usize
        } else {
            index as usize
        };
        arr.get(actual_index)
    } else {
        // Object field
        value.get(segment)
    }
}

/// Parse the Link header to find the URL with rel="next".
/// Uses bracket-aware splitting to handle commas inside URL angle brackets.
/// Parses parameters per RFC 5988 (split on `;`, trim, exact-match `rel="next"`).
fn parse_link_header_next(headers: &HeaderMap) -> Option<String> {
    let link_header = headers.get("link")?.to_str().ok()?;

    // Split on commas that are outside angle brackets
    for part in split_link_header(link_header) {
        let part = part.trim();
        // Parse parameters per RFC 5988: split on ';' and check each param
        if has_rel_next(part) {
            // Extract URL between < and >
            if let Some(start) = part.find('<') {
                if let Some(end) = part.find('>') {
                    return Some(part[start + 1..end].to_string());
                }
            }
        }
    }

    None
}

/// Check if a Link header part has an exact `rel="next"` or `rel='next'` parameter.
/// Splits on `;` per RFC 5988 and performs exact-match on each parameter value.
fn has_rel_next(part: &str) -> bool {
    for param in part.split(';') {
        let param = param.trim();
        if param.eq_ignore_ascii_case("rel=\"next\"") || param.eq_ignore_ascii_case("rel='next'") {
            return true;
        }
    }
    false
}

/// Split a Link header value on commas that are outside angle brackets.
fn split_link_header(header: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0u32;
    let mut start = 0;

    for (i, c) in header.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&header[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&header[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_simple_path() {
        let data = json!({"data": {"total": 100}});
        assert_eq!(extract_json_path_u64(&data, "$.data.total"), Some(100));
    }

    #[test]
    fn test_extract_array_last() {
        let data = json!({"data": [{"id": "a"}, {"id": "b"}, {"id": "c"}]});
        assert_eq!(
            extract_json_path_string(&data, "$.data[-1].id"),
            Some("c".to_string())
        );
    }

    #[test]
    fn test_extract_bool() {
        let data = json!({"has_more": true});
        assert_eq!(extract_json_path_bool(&data, "$.has_more"), Some(true));
    }

    #[test]
    fn test_extract_missing_path() {
        let data = json!({"data": {}});
        assert_eq!(extract_json_path_string(&data, "$.nonexistent"), None);
    }

    #[test]
    fn test_parse_link_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "link",
            r#"<https://api.github.com/repos?page=3>; rel="next", <https://api.github.com/repos?page=50>; rel="last""#
                .parse()
                .unwrap(),
        );
        assert_eq!(
            parse_link_header_next(&headers),
            Some("https://api.github.com/repos?page=3".to_string())
        );
    }

    #[test]
    fn test_parse_link_header_no_next() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "link",
            r#"<https://api.github.com/repos?page=1>; rel="first""#
                .parse()
                .unwrap(),
        );
        assert_eq!(parse_link_header_next(&headers), None);
    }

    #[test]
    fn test_offset_limit_paginator() {
        let config = PaginationConfig::OffsetLimit {
            offset_param: "offset".to_string(),
            limit_param: "limit".to_string(),
            page_size: 10,
            total_path: None,
        };

        let mut paginator = create_paginator(&config, "example.com".to_string());
        let initial = paginator.initial_params();
        assert_eq!(
            initial,
            vec![
                ("offset".to_string(), "0".to_string()),
                ("limit".to_string(), "10".to_string())
            ]
        );

        // Full page → should have next
        let headers = HeaderMap::new();
        let body = json!({});
        let next = paginator.next_page(&body, &headers, 10).unwrap();
        assert!(next.is_some());

        // Partial page → should be done
        let next = paginator.next_page(&body, &headers, 5).unwrap();
        assert!(next.is_none());
    }

    #[test]
    fn test_cursor_paginator_with_has_more() {
        let config = PaginationConfig::Cursor {
            cursor_param: "starting_after".to_string(),
            cursor_path: "$.data[-1].id".to_string(),
            has_more_path: Some("$.has_more".to_string()),
            page_size_param: Some("limit".to_string()),
            page_size: Some(10),
        };

        let mut paginator = create_paginator(&config, "example.com".to_string());

        let headers = HeaderMap::new();
        let body = json!({"data": [{"id": "a"}, {"id": "b"}], "has_more": true});
        let next = paginator.next_page(&body, &headers, 2).unwrap();
        assert!(next.is_some());

        let body = json!({"data": [{"id": "c"}], "has_more": false});
        let next = paginator.next_page(&body, &headers, 1).unwrap();
        assert!(next.is_none());
    }

    #[test]
    fn test_next_url_paginator() {
        let config = PaginationConfig::NextUrl {
            next_url_path: "$.nextRecordsUrl".to_string(),
            base_url: Some("https://instance.salesforce.com".to_string()),
        };

        let mut paginator = create_paginator(&config, "instance.salesforce.com".to_string());
        let headers = HeaderMap::new();

        let body = json!({"nextRecordsUrl": "/services/data/v56.0/query/abc-123"});
        let next = paginator.next_page(&body, &headers, 10).unwrap();
        match next {
            Some(NextPage::NewUrl(url)) => {
                assert_eq!(
                    url,
                    "https://instance.salesforce.com/services/data/v56.0/query/abc-123"
                );
            }
            _ => panic!("Expected NewUrl"),
        }

        // No next URL → done
        let body = json!({"records": []});
        let next = paginator.next_page(&body, &headers, 0).unwrap();
        assert!(next.is_none());
    }

    #[test]
    fn test_negative_index_out_of_bounds() {
        let data = json!({"data": [{"id": "a"}, {"id": "b"}]});
        // -3 on a 2-element array should return None, not wrap
        assert_eq!(extract_json_path_string(&data, "$.data[-3].id"), None);
        // -2 should work (first element)
        assert_eq!(
            extract_json_path_string(&data, "$.data[-2].id"),
            Some("a".to_string())
        );
    }

    #[test]
    fn test_navigate_path_top_level_array() {
        let data = json!([{"id": "1"}, {"id": "2"}]);
        let result = navigate_path(&data, "$");
        assert!(result.is_some());
        assert!(result.unwrap().is_array());
    }

    #[test]
    fn test_ssrf_protection_rejects_different_host() {
        let config = PaginationConfig::NextUrl {
            next_url_path: "$.next".to_string(),
            base_url: None,
        };

        let mut paginator = create_paginator(&config, "api.example.com".to_string());
        let headers = HeaderMap::new();

        // Attacker injects an internal URL in the response
        let body = json!({"next": "http://169.254.169.254/latest/meta-data/"}); // DevSkim: ignore DS137138
        let result = paginator.next_page(&body, &headers, 10);
        assert!(result.is_err(), "Should reject URL to different host");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("SSRF protection"),
            "Error should mention SSRF: {err_msg}"
        );
    }

    #[test]
    fn test_ssrf_protection_allows_same_host() {
        let config = PaginationConfig::NextUrl {
            next_url_path: "$.next".to_string(),
            base_url: None,
        };

        let mut paginator = create_paginator(&config, "api.example.com".to_string());
        let headers = HeaderMap::new();

        let body = json!({"next": "https://api.example.com/page/2"});
        let result = paginator.next_page(&body, &headers, 10).unwrap();
        assert!(matches!(result, Some(NextPage::NewUrl(_))));
    }

    #[test]
    fn test_ssrf_protection_rejects_non_http_scheme() {
        let config = PaginationConfig::NextUrl {
            next_url_path: "$.next".to_string(),
            base_url: None,
        };

        let mut paginator = create_paginator(&config, "api.example.com".to_string());
        let headers = HeaderMap::new();

        let body = json!({"next": "file:///etc/passwd"});
        let result = paginator.next_page(&body, &headers, 10);
        assert!(result.is_err(), "Should reject non-HTTP scheme");
    }
}
