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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StartBehavior {
    #[default]
    StartFromNow,
    StartFromBeginning,
    StartFromTimestamp(i64),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CategoryConfig {
    pub outages: bool,
    pub bgp_hijacks: bool,
    pub bgp_leaks: bool,
    pub http_traffic: bool,
    pub attacks_l7: bool,
    pub attacks_l3: bool,
    pub domain_rankings: bool,
    pub dns: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CloudflareRadarConfig {
    pub api_token: String,
    pub api_base_url: String,
    pub poll_interval_secs: u64,
    pub categories: CategoryConfig,
    pub location_filter: Option<Vec<String>>,
    pub asn_filter: Option<Vec<u32>>,
    pub hijack_min_confidence: Option<u32>,
    pub ranking_limit: u32,
    pub dns_domains: Option<Vec<String>>,
    pub analytics_date_range: String,
    pub event_date_range: String,
    pub start_behavior: StartBehavior,
}

impl Default for CloudflareRadarConfig {
    fn default() -> Self {
        Self {
            api_token: String::new(),
            api_base_url: "https://api.cloudflare.com/client/v4".to_string(),
            poll_interval_secs: 300,
            categories: CategoryConfig::default(),
            location_filter: None,
            asn_filter: None,
            hijack_min_confidence: None,
            ranking_limit: 100,
            dns_domains: None,
            analytics_date_range: "1d".to_string(),
            event_date_range: "7d".to_string(),
            start_behavior: StartBehavior::StartFromNow,
        }
    }
}
