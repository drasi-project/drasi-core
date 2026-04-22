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

use crate::api::{BgpHijackEvent, BgpLeakEvent, OutageAnnotation};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PollingState {
    pub initialized: bool,
    pub seen_hijack_ids: HashMap<u64, BgpHijackEvent>,
    pub seen_leak_ids: HashMap<u64, BgpLeakEvent>,
    pub seen_outage_ids: HashMap<String, OutageAnnotation>,
    pub last_http_hash: Option<u64>,
    pub last_l7_hash: Option<u64>,
    pub last_l3_hash: Option<u64>,
    pub last_dns_hash: HashMap<String, u64>,
    pub known_rankings: HashMap<String, u32>,
}

impl PollingState {
    pub fn new() -> Self {
        Self::default()
    }
}
