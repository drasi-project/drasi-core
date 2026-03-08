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

//! Deserialization types for Hyperliquid REST and WebSocket APIs.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct InfoRequest {
    #[serde(rename = "type")]
    pub req_type: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PerpAsset {
    pub name: String,
    pub sz_decimals: u32,
    pub max_leverage: u32,
    #[serde(default)]
    pub is_delisted: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MetaResponse {
    pub universe: Vec<PerpAsset>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotPair {
    pub name: String,
    pub tokens: Vec<u32>,
    #[serde(default)]
    pub index: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotToken {
    pub name: String,
    pub sz_decimals: u32,
    pub wei_decimals: u32,
    pub index: u32,
    #[serde(default)]
    pub token_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpotMetaResponse {
    pub universe: Vec<SpotPair>,
    pub tokens: Vec<SpotToken>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetCtx {
    pub funding: String,
    pub open_interest: String,
    pub prev_day_px: String,
    pub day_ntl_vlm: String,
    pub premium: String,
    pub oracle_px: String,
    pub mark_px: String,
    pub mid_px: String,
    pub impact_pxs: Vec<String>,
    pub day_base_vlm: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L2Level {
    pub px: String,
    pub sz: String,
    pub n: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct L2Book {
    pub coin: String,
    pub time: i64,
    pub levels: Vec<Vec<L2Level>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Trade {
    pub coin: String,
    pub side: String,
    pub px: String,
    pub sz: String,
    pub time: i64,
    #[serde(default)]
    pub hash: Option<String>,
    pub tid: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Liquidation {
    pub coin: String,
    pub side: String,
    pub px: String,
    pub sz: String,
    pub time: i64,
    #[serde(default)]
    pub hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WsMessage {
    pub channel: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FundingSnapshot {
    pub rate: f64,
    pub premium: f64,
    pub mark_price: f64,
    pub open_interest: f64,
    pub volume_24h: f64,
    pub timestamp: i64,
}
