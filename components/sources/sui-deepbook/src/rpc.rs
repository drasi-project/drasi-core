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

use anyhow::{anyhow, Context, Result};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EventCursor {
    #[serde(rename = "txDigest")]
    pub tx_digest: String,
    #[serde(
        rename = "eventSeq",
        deserialize_with = "deserialize_string_or_number_as_string"
    )]
    pub event_seq: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuiEvent {
    pub id: EventCursor,
    pub package_id: String,
    pub transaction_module: String,
    pub sender: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub parsed_json: Value,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    pub timestamp_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryEventsResult {
    pub data: Vec<SuiEvent>,
    pub next_cursor: Option<EventCursor>,
    pub has_next_page: bool,
}

#[derive(Debug, Deserialize)]
struct RpcEnvelope<T> {
    result: Option<T>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct SuiGetObjectResult {
    data: Option<SuiObjectData>,
}

/// Top-level object data returned by `sui_getObject`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuiObjectData {
    pub object_id: String,
    /// Full Move type path including type parameters,
    /// e.g. `"0x…::pool::Pool<0x2::sui::SUI, 0x…::usdc::USDC>"`.
    #[serde(rename = "type")]
    pub object_type: Option<String>,
    pub content: Option<SuiObjectContent>,
}

/// Parsed Move object content.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuiObjectContent {
    pub data_type: String,
    pub fields: Option<Value>,
}

impl SuiObjectData {
    /// Extract type parameters from the Move type path.
    ///
    /// For a type like `0x…::pool::Pool<0x2::sui::SUI, 0x…::usdc::USDC>`
    /// this returns `["0x2::sui::SUI", "0x…::usdc::USDC"]`.
    pub fn type_params(&self) -> Vec<String> {
        let Some(ref type_str) = self.object_type else {
            return Vec::new();
        };
        let Some(start) = type_str.find('<') else {
            return Vec::new();
        };
        let Some(end) = type_str.rfind('>') else {
            return Vec::new();
        };
        if start >= end {
            return Vec::new();
        }
        let inner = &type_str[start + 1..end];
        // Split on ", " while respecting nested generics
        let mut params = Vec::new();
        let mut depth = 0usize;
        let mut current_start = 0;
        for (i, ch) in inner.char_indices() {
            match ch {
                '<' => depth += 1,
                '>' => {
                    depth = depth.saturating_sub(1);
                }
                ',' if depth == 0 => {
                    params.push(inner[current_start..i].trim().to_string());
                    current_start = i + 1;
                }
                _ => {}
            }
        }
        let last = inner[current_start..].trim();
        if !last.is_empty() {
            params.push(last.to_string());
        }
        params
    }

    /// Extract a string field from the object's content fields.
    pub fn field_str(&self, key: &str) -> Option<String> {
        let fields = self.content.as_ref()?.fields.as_ref()?;
        match fields.get(key)? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SuiRpcClient {
    endpoint: String,
    client: reqwest::Client,
}

impl SuiRpcClient {
    pub fn new(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        reqwest::Url::parse(&endpoint)
            .map_err(|e| anyhow!("Invalid Sui RPC endpoint '{endpoint}': {e}"))?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("Failed to build HTTP client for Sui RPC")?;
        Ok(Self { endpoint, client })
    }

    /// Fetch a Sui object by ID using `sui_getObject`.
    ///
    /// Returns the parsed content fields so callers can extract pool metadata
    /// such as `base_asset`, `quote_asset`, `tick_size`, `lot_size`, etc.
    pub async fn get_object(&self, object_id: &str) -> Result<SuiObjectData> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sui_getObject",
            "params": [object_id, { "showContent": true, "showType": true }],
        });

        let response = self
            .client
            .post(&self.endpoint)
            .json(&request)
            .send()
            .await
            .context("Failed to send sui_getObject request")?;

        let status = response.status();
        let body: RpcEnvelope<SuiGetObjectResult> = response
            .json()
            .await
            .with_context(|| format!("Failed to parse sui_getObject response (status {status})"))?;

        if let Some(error) = body.error {
            return Err(anyhow!(
                "Sui RPC error {} from sui_getObject: {}",
                error.code,
                error.message
            ));
        }

        let result = body
            .result
            .ok_or_else(|| anyhow!("Sui RPC response missing result for sui_getObject"))?;

        result
            .data
            .ok_or_else(|| anyhow!("sui_getObject returned no data for '{object_id}'"))
    }

    pub async fn query_events(
        &self,
        query_filter: Value,
        cursor: Option<&EventCursor>,
        limit: u16,
        descending_order: bool,
    ) -> Result<QueryEventsResult> {
        let cursor_value = match cursor {
            Some(c) => serde_json::to_value(c)?,
            None => Value::Null,
        };

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "suix_queryEvents",
            "params": [query_filter, cursor_value, limit, descending_order],
        });

        let response = self
            .client
            .post(&self.endpoint)
            .json(&request)
            .send()
            .await
            .context("Failed to send suix_queryEvents request")?;

        let status = response.status();
        let body: RpcEnvelope<QueryEventsResult> = response.json().await.with_context(|| {
            format!("Failed to parse suix_queryEvents response (status {status})")
        })?;

        if let Some(error) = body.error {
            return Err(anyhow!(
                "Sui RPC error {} from suix_queryEvents: {}",
                error.code,
                error.message
            ));
        }

        body.result
            .ok_or_else(|| anyhow!("Sui RPC response missing result for suix_queryEvents"))
    }
}

fn deserialize_optional_u64<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Null => Ok(None),
        Value::Number(num) => num
            .as_u64()
            .ok_or_else(|| D::Error::custom("timestampMs number is not a valid u64"))
            .map(Some),
        Value::String(text) => text
            .parse::<u64>()
            .map(Some)
            .map_err(|e| D::Error::custom(format!("invalid timestampMs '{text}': {e}"))),
        _ => Err(D::Error::custom(
            "timestampMs must be a string, number, or null",
        )),
    }
}

fn deserialize_string_or_number_as_string<'de, D>(
    deserializer: D,
) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::String(text) => Ok(text),
        Value::Number(num) => Ok(num.to_string()),
        _ => Err(D::Error::custom(
            "Expected string or number for value conversion",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserializes_event_seq_number() {
        let raw = serde_json::json!({
            "txDigest": "abc",
            "eventSeq": 12
        });
        let cursor: EventCursor = serde_json::from_value(raw).unwrap();
        assert_eq!(cursor.event_seq, "12");
    }

    #[test]
    fn test_deserializes_timestamp_string() {
        let raw = serde_json::json!({
            "id": {"txDigest": "tx", "eventSeq": "1"},
            "packageId": "0x1",
            "transactionModule": "pool",
            "sender": "0x2",
            "type": "0x1::events::OrderPlaced",
            "parsedJson": {"order_id": "1"},
            "timestampMs": "1772923888171"
        });
        let event: SuiEvent = serde_json::from_value(raw).unwrap();
        assert_eq!(event.timestamp_ms, Some(1_772_923_888_171));
    }

    #[test]
    fn test_deserializes_object_response() {
        let raw = serde_json::json!({
            "objectId": "0xpool123",
            "type": "0xdee9::pool::Pool<0x2::sui::SUI, 0xabc::usdc::USDC>",
            "content": {
                "dataType": "moveObject",
                "fields": {
                    "tick_size": "1000000",
                    "lot_size": "100000000",
                    "min_size": "500000000"
                }
            }
        });
        let data: SuiObjectData = serde_json::from_value(raw).unwrap();
        assert_eq!(data.object_id, "0xpool123");
        assert_eq!(data.field_str("tick_size"), Some("1000000".to_string()));
        assert_eq!(data.field_str("lot_size"), Some("100000000".to_string()));
        assert_eq!(data.field_str("min_size"), Some("500000000".to_string()));
        assert_eq!(data.field_str("nonexistent"), None);
    }

    #[test]
    fn test_type_params_extraction() {
        let data = SuiObjectData {
            object_id: "0xpool".to_string(),
            object_type: Some("0xdee9::pool::Pool<0x2::sui::SUI, 0xabc::usdc::USDC>".to_string()),
            content: None,
        };
        let params = data.type_params();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], "0x2::sui::SUI");
        assert_eq!(params[1], "0xabc::usdc::USDC");
    }

    #[test]
    fn test_type_params_no_generics() {
        let data = SuiObjectData {
            object_id: "0x1".to_string(),
            object_type: Some("0x2::coin::Coin".to_string()),
            content: None,
        };
        assert!(data.type_params().is_empty());
    }

    #[test]
    fn test_type_params_nested_generics() {
        let data = SuiObjectData {
            object_id: "0x1".to_string(),
            object_type: Some(
                "0xdee9::pool::Pool<0x2::coin::Coin<0x3::foo::Bar>, 0x4::usdc::USDC>".to_string(),
            ),
            content: None,
        };
        let params = data.type_params();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], "0x2::coin::Coin<0x3::foo::Bar>");
        assert_eq!(params[1], "0x4::usdc::USDC");
    }
}
