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

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudflareResponse<T> {
    pub success: bool,
    pub errors: Vec<serde_json::Value>,
    pub result: T,
}

// -----------------------------------------------------------------------------
// Outages
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OutageAnnotation {
    pub data_source: Option<String>,
    pub description: Option<String>,
    pub scope: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub locations: Vec<String>,
    pub asns: Vec<u32>,
    pub event_type: Option<String>,
    pub linked_url: Option<String>,
    pub outage: Option<OutageDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OutageDetails {
    pub outage_cause: Option<String>,
    pub outage_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutageResult {
    pub annotations: Vec<OutageAnnotation>,
}

// -----------------------------------------------------------------------------
// BGP Hijacks
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BgpHijackEvent {
    pub id: u64,
    pub duration: Option<u64>,
    pub event_type: Option<u32>,
    pub hijack_msgs_count: Option<u64>,
    pub hijacker_asn: Option<u32>,
    pub is_stale: Option<bool>,
    pub max_hijack_ts: Option<String>,
    pub min_hijack_ts: Option<String>,
    pub on_going_count: Option<u64>,
    pub peer_asns: Option<Vec<u32>>,
    pub peer_ip_count: Option<u64>,
    pub prefixes: Option<Vec<String>>,
    pub tags: Option<Vec<HijackTag>>,
    pub victim_asns: Option<Vec<u32>>,
    pub confidence_score: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HijackTag {
    pub name: String,
    pub score: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BgpHijackResult {
    pub asn_info: Option<Vec<AsnInfo>>,
    pub events: Vec<BgpHijackEvent>,
    pub total_monitors: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AsnInfo {
    pub asn: u32,
    pub org_name: Option<String>,
    pub country_code: Option<String>,
}

// -----------------------------------------------------------------------------
// BGP Leaks
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BgpLeakEvent {
    pub id: u64,
    pub detected_ts: Option<String>,
    pub finished: Option<bool>,
    pub leak_asn: Option<u32>,
    pub leak_count: Option<u64>,
    pub leak_seg: Option<Vec<u32>>,
    pub leak_type: Option<u32>,
    pub max_ts: Option<String>,
    pub min_ts: Option<String>,
    pub origin_count: Option<u64>,
    pub peer_count: Option<u64>,
    pub prefix_count: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BgpLeakResult {
    pub asn_info: Option<Vec<AsnInfo>>,
    pub events: Vec<BgpLeakEvent>,
}

// -----------------------------------------------------------------------------
// HTTP Traffic Summary
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpSummaryResult {
    #[serde(flatten)]
    pub summaries: HashMap<String, HttpSummaryData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpSummaryData {
    pub desktop: Option<String>,
    pub mobile: Option<String>,
    pub other: Option<String>,
}

// -----------------------------------------------------------------------------
// Layer 7 Attacks Summary
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttackSummaryResult {
    #[serde(flatten)]
    pub summaries: HashMap<String, AttackSummaryData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttackSummaryData {
    pub ddos: Option<String>,
    pub waf: Option<String>,
    pub ip_reputation: Option<String>,
    pub access_rules: Option<String>,
    pub bot_management: Option<String>,
    pub api_shield: Option<String>,
    pub data_loss_prevention: Option<String>,
}

// -----------------------------------------------------------------------------
// Layer 3 Attacks Summary
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttackLayer3SummaryResult {
    #[serde(flatten)]
    pub summaries: HashMap<String, AttackLayer3SummaryData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttackLayer3SummaryData {
    pub udp: Option<String>,
    pub tcp: Option<String>,
    pub icmp: Option<String>,
    pub gre: Option<String>,
}

// -----------------------------------------------------------------------------
// Domain Rankings
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankingResult {
    #[serde(flatten)]
    pub rankings: HashMap<String, Vec<DomainRank>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DomainRank {
    pub rank: u32,
    pub domain: String,
}

// -----------------------------------------------------------------------------
// DNS Top Locations
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsTopLocationsResult {
    #[serde(flatten)]
    pub top: HashMap<String, Vec<DnsLocationEntry>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DnsLocationEntry {
    pub client_country_alpha2: Option<String>,
    pub client_country_name: Option<String>,
    pub value: Option<String>,
}
