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

//! Configuration types for the HERE Traffic source.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Authentication method for the HERE Traffic API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthMethod {
    /// Simple API key passed as `apiKey` query parameter.
    #[serde(rename = "api_key")]
    ApiKey { api_key: String },
    /// OAuth 2.0 client credentials using access key ID and secret.
    /// Exchanges credentials for a bearer token via the HERE token endpoint.
    #[serde(rename = "oauth")]
    OAuth {
        access_key_id: String,
        access_key_secret: String,
        #[serde(default = "default_token_url")]
        token_url: String,
    },
}

fn default_token_url() -> String {
    "https://account.api.here.com/oauth2/token".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoundingBox {
    pub lat1: f64,
    pub lon1: f64,
    pub lat2: f64,
    pub lon2: f64,
}

impl BoundingBox {
    pub fn parse(value: &str) -> Result<Self> {
        let parts: Vec<&str> = value.split(',').map(|s| s.trim()).collect();
        if parts.len() != 4 {
            return Err(anyhow::anyhow!(
                "Validation error: bounding_box must have 4 comma-separated values (lat1,lon1,lat2,lon2)"
            ));
        }

        let lat1: f64 = parts[0].parse()?;
        let lon1: f64 = parts[1].parse()?;
        let lat2: f64 = parts[2].parse()?;
        let lon2: f64 = parts[3].parse()?;

        if !(-90.0..=90.0).contains(&lat1)
            || !(-90.0..=90.0).contains(&lat2)
            || !(-180.0..=180.0).contains(&lon1)
            || !(-180.0..=180.0).contains(&lon2)
        {
            return Err(anyhow::anyhow!(
                "Validation error: bounding_box coordinates out of range (-90..90 latitude, -180..180 longitude)"
            ));
        }

        Ok(Self {
            lat1,
            lon1,
            lat2,
            lon2,
        })
    }

    /// Convert to HERE Traffic API format: `west_lon,south_lat,east_lon,north_lat`.
    pub fn as_here_format(&self) -> String {
        let west = self.lon1.min(self.lon2);
        let south = self.lat1.min(self.lat2);
        let east = self.lon1.max(self.lon2);
        let north = self.lat1.max(self.lat2);
        format!("{west},{south},{east},{north}")
    }
}

/// API endpoints to poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Endpoint {
    Flow,
    Incidents,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StartFrom {
    #[serde(rename = "now")]
    #[default]
    Now,
    #[serde(rename = "timestamp")]
    Timestamp { value: i64 },
}

/// Configuration for HERE Traffic source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HereTrafficConfig {
    /// Authentication method (API key or OAuth 2.0)
    pub auth: AuthMethod,
    /// Bounding box in "lat1,lon1,lat2,lon2" format
    pub bounding_box: String,
    /// Polling interval in seconds
    #[serde(default = "default_polling_interval")]
    pub polling_interval: u64,
    /// Endpoints to poll
    #[serde(default = "default_endpoints")]
    pub endpoints: Vec<Endpoint>,
    /// Minimum jam factor change to report
    #[serde(default = "default_flow_change_threshold")]
    pub flow_change_threshold: f64,
    /// Minimum speed change to report (km/h)
    #[serde(default = "default_speed_change_threshold")]
    pub speed_change_threshold: f64,
    /// Maximum distance between incident and segment to create AFFECTS relation (meters)
    #[serde(default = "default_relation_distance_meters")]
    pub incident_match_distance_meters: f64,
    /// Start behavior when no cursor is found
    #[serde(default)]
    pub start_from: StartFrom,
    /// Base URL for HERE Traffic API (override for testing)
    #[serde(default = "default_base_url")]
    pub base_url: String,
}

impl HereTrafficConfig {
    /// Create config with API key authentication.
    pub fn new(api_key: impl Into<String>, bounding_box: impl Into<String>) -> Self {
        Self {
            auth: AuthMethod::ApiKey {
                api_key: api_key.into(),
            },
            bounding_box: bounding_box.into(),
            polling_interval: default_polling_interval(),
            endpoints: default_endpoints(),
            flow_change_threshold: default_flow_change_threshold(),
            speed_change_threshold: default_speed_change_threshold(),
            incident_match_distance_meters: default_relation_distance_meters(),
            start_from: StartFrom::default(),
            base_url: default_base_url(),
        }
    }

    /// Create config with OAuth 2.0 authentication.
    pub fn new_oauth(
        access_key_id: impl Into<String>,
        access_key_secret: impl Into<String>,
        bounding_box: impl Into<String>,
    ) -> Self {
        Self {
            auth: AuthMethod::OAuth {
                access_key_id: access_key_id.into(),
                access_key_secret: access_key_secret.into(),
                token_url: default_token_url(),
            },
            bounding_box: bounding_box.into(),
            polling_interval: default_polling_interval(),
            endpoints: default_endpoints(),
            flow_change_threshold: default_flow_change_threshold(),
            speed_change_threshold: default_speed_change_threshold(),
            incident_match_distance_meters: default_relation_distance_meters(),
            start_from: StartFrom::default(),
            base_url: default_base_url(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        match &self.auth {
            AuthMethod::ApiKey { api_key } => {
                if api_key.trim().is_empty() {
                    return Err(anyhow::anyhow!("Validation error: api_key cannot be empty"));
                }
            }
            AuthMethod::OAuth {
                access_key_id,
                access_key_secret,
                token_url,
            } => {
                if access_key_id.trim().is_empty() {
                    return Err(anyhow::anyhow!(
                        "Validation error: access_key_id cannot be empty"
                    ));
                }
                if access_key_secret.trim().is_empty() {
                    return Err(anyhow::anyhow!(
                        "Validation error: access_key_secret cannot be empty"
                    ));
                }
                let _ = reqwest::Url::parse(token_url).map_err(|e| {
                    anyhow::anyhow!(
                        "Validation error: token_url is not a valid URL ({token_url}): {e}"
                    )
                })?;
            }
        }

        BoundingBox::parse(&self.bounding_box)?;

        if self.polling_interval == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: polling_interval must be greater than 0"
            ));
        }

        if self.endpoints.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: at least one endpoint (flow/incidents) must be configured"
            ));
        }

        if self.flow_change_threshold < 0.0 {
            return Err(anyhow::anyhow!(
                "Validation error: flow_change_threshold cannot be negative"
            ));
        }

        if self.speed_change_threshold < 0.0 {
            return Err(anyhow::anyhow!(
                "Validation error: speed_change_threshold cannot be negative"
            ));
        }

        if self.incident_match_distance_meters <= 0.0 {
            return Err(anyhow::anyhow!(
                "Validation error: incident_match_distance_meters must be positive"
            ));
        }

        let _ = reqwest::Url::parse(&self.base_url).map_err(|e| {
            anyhow::anyhow!(
                "Validation error: base_url is not a valid URL ({base_url}): {e}",
                base_url = self.base_url
            )
        })?;

        Ok(())
    }
}

fn default_polling_interval() -> u64 {
    60
}

fn default_endpoints() -> Vec<Endpoint> {
    vec![Endpoint::Flow, Endpoint::Incidents]
}

fn default_flow_change_threshold() -> f64 {
    0.5
}

fn default_speed_change_threshold() -> f64 {
    5.0
}

fn default_relation_distance_meters() -> f64 {
    500.0
}

fn default_base_url() -> String {
    "https://data.traffic.hereapi.com".to_string()
}
