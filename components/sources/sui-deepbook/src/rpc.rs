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
}
