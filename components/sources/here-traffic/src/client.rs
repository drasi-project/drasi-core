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

//! HTTP client and response types for HERE Traffic API.

use crate::config::{AuthMethod, BoundingBox};
use anyhow::Result;
use log::{debug, info, warn};
use reqwest::header::RETRY_AFTER;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const MAX_BACKOFF_SECS: u64 = 60;
const TOKEN_REFRESH_MARGIN_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Flow API response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FlowResponse {
    #[serde(default)]
    pub results: Vec<FlowResult>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FlowResult {
    pub location: Option<FlowResultLocation>,
    #[serde(rename = "currentFlow")]
    pub current_flow: Option<CurrentFlow>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FlowResultLocation {
    pub description: Option<String>,
    pub length: Option<f64>,
    pub shape: Option<Shape>,
}

/// Current traffic flow data for a road segment.
/// Note: speed values are in **m/s** as returned by the HERE API.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CurrentFlow {
    pub speed: Option<f64>,
    #[serde(rename = "speedUncapped")]
    pub speed_uncapped: Option<f64>,
    #[serde(rename = "freeFlow")]
    pub free_flow: Option<f64>,
    #[serde(rename = "jamFactor")]
    pub jam_factor: Option<f64>,
    pub confidence: Option<f64>,
    pub traversability: Option<String>,
}

// ---------------------------------------------------------------------------
// Incidents API response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct IncidentsResponse {
    #[serde(default)]
    pub results: Vec<IncidentResult>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct IncidentResult {
    #[serde(rename = "incidentDetails")]
    pub incident_details: Option<IncidentDetails>,
    pub location: Option<IncidentResultLocation>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct IncidentDetails {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub incident_type: Option<String>,
    pub summary: Option<TextValue>,
    pub description: Option<TextValue>,
    #[serde(rename = "typeDescription")]
    pub type_description: Option<TextValue>,
    pub severity: Option<serde_json::Value>,
    pub criticality: Option<String>,
    #[serde(rename = "roadClosed")]
    pub road_closed: Option<bool>,
    #[serde(rename = "roadInfo")]
    pub road_info: Option<RoadInfo>,
    #[serde(rename = "startTime")]
    pub start_time: Option<String>,
    #[serde(rename = "endTime")]
    pub end_time: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TextValue {
    pub value: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RoadInfo {
    pub name: Option<String>,
    pub id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct IncidentResultLocation {
    pub description: Option<String>,
    pub shape: Option<Shape>,
}

// ---------------------------------------------------------------------------
// Shared geometry types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Shape {
    pub links: Option<Vec<ShapeLink>>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ShapeLink {
    pub points: Option<Vec<ShapePoint>>,
    pub length: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ShapePoint {
    pub lat: f64,
    pub lng: f64,
}

/// Cached OAuth bearer token.
struct OAuthTokenCache {
    access_token: String,
    expires_at: std::time::Instant,
}

/// Response from the HERE OAuth 2.0 token endpoint.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: u64,
}

#[derive(Clone)]
pub struct HereTrafficClient {
    auth: AuthMethod,
    base_url: String,
    client: reqwest::Client,
    token_cache: Arc<Mutex<Option<OAuthTokenCache>>>,
}

impl std::fmt::Debug for HereTrafficClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HereTrafficClient")
            .field("base_url", &self.base_url)
            .field("auth", &"<redacted>")
            .finish()
    }
}

impl HereTrafficClient {
    pub fn new(auth: AuthMethod, base_url: impl Into<String>) -> Result<Self> {
        let base_url = base_url.into();
        reqwest::Url::parse(&base_url)?;

        Ok(Self {
            auth,
            base_url,
            client: reqwest::Client::new(),
            token_cache: Arc::new(Mutex::new(None)),
        })
    }

    pub async fn get_flow(&self, bbox: &str) -> Result<FlowResponse> {
        let url = format!("{}/v7/flow", self.base_url.trim_end_matches('/'));
        let here_bbox = BoundingBox::parse(bbox)?.as_here_format();
        self.send_with_backoff(&url, &here_bbox).await
    }

    pub async fn get_incidents(&self, bbox: &str) -> Result<IncidentsResponse> {
        let url = format!("{}/v7/incidents", self.base_url.trim_end_matches('/'));
        let here_bbox = BoundingBox::parse(bbox)?.as_here_format();
        self.send_with_backoff(&url, &here_bbox).await
    }

    async fn send_with_backoff<T>(&self, url: &str, bbox: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        const MAX_RETRIES: u32 = 5;
        let mut backoff = Duration::from_secs(1);
        let mut token_retried = false;
        let mut retries: u32 = 0;

        loop {
            let mut request = self.client.get(url).query(&[
                ("in", format!("bbox:{bbox}")),
                ("locationReferencing", "shape".to_string()),
            ]);

            match &self.auth {
                AuthMethod::ApiKey { api_key } => {
                    request = request.query(&[("apiKey", api_key.clone())]);
                }
                AuthMethod::OAuth { .. } => {
                    let token = self.get_bearer_token().await?;
                    request = request.bearer_auth(token);
                }
            }

            let response = request.send().await?;

            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                retries += 1;
                if retries > MAX_RETRIES {
                    return Err(anyhow::anyhow!(
                        "HERE Traffic API rate limit exceeded after {MAX_RETRIES} retries"
                    ));
                }
                let retry_after = parse_retry_after(response.headers().get(RETRY_AFTER));
                let wait = retry_after.unwrap_or(backoff);
                warn!(
                    "HERE Traffic API rate limit hit (429). Retry {retries}/{MAX_RETRIES}, backing off for {:.1}s",
                    wait.as_secs_f64()
                );
                tokio::time::sleep(wait).await;
                backoff = Duration::from_secs((backoff.as_secs() * 2).min(MAX_BACKOFF_SECS));
                continue;
            }

            // For OAuth, a 401 may mean the token expired; clear cache and retry once.
            if response.status() == reqwest::StatusCode::UNAUTHORIZED
                && matches!(&self.auth, AuthMethod::OAuth { .. })
                && !token_retried
            {
                warn!("HERE Traffic API returned 401; refreshing OAuth token");
                *self.token_cache.lock().await = None;
                token_retried = true;
                continue;
            }

            let status = response.status();
            let text = response.text().await?;
            debug!(
                "HERE Traffic API response status: {status}, body length: {}",
                text.len()
            );
            if log::log_enabled!(log::Level::Trace) {
                log::trace!(
                    "HERE Traffic API raw response: {}",
                    &text[..text.len().min(2000)]
                );
            }

            if !status.is_success() {
                return Err(anyhow::anyhow!(
                    "HERE Traffic API request failed with status {status}: {text}"
                ));
            }

            let parsed: T = serde_json::from_str(&text)?;
            return Ok(parsed);
        }
    }

    /// Get a valid bearer token, using the cache when possible.
    async fn get_bearer_token(&self) -> Result<String> {
        let (access_key_id, access_key_secret, token_url) = match &self.auth {
            AuthMethod::OAuth {
                access_key_id,
                access_key_secret,
                token_url,
            } => (access_key_id, access_key_secret, token_url),
            _ => return Err(anyhow::anyhow!("OAuth auth not configured")),
        };

        // Hold lock for the entire check-then-fetch to avoid duplicate requests.
        let mut cache = self.token_cache.lock().await;
        if let Some(cached) = cache.as_ref() {
            if cached.expires_at > std::time::Instant::now() {
                return Ok(cached.access_token.clone());
            }
        }

        info!("Fetching new OAuth bearer token from HERE");
        let token_response =
            fetch_oauth_token(&self.client, access_key_id, access_key_secret, token_url).await?;

        let access_token = token_response.access_token.clone();
        let ttl = token_response.expires_in.max(TOKEN_REFRESH_MARGIN_SECS + 1);
        let expires_at =
            std::time::Instant::now() + Duration::from_secs(ttl - TOKEN_REFRESH_MARGIN_SECS);

        *cache = Some(OAuthTokenCache {
            access_token: access_token.clone(),
            expires_at,
        });

        Ok(access_token)
    }
}

fn parse_retry_after(header: Option<&reqwest::header::HeaderValue>) -> Option<Duration> {
    let header = header?.to_str().ok()?;
    if let Ok(seconds) = header.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    None
}

// ---------------------------------------------------------------------------
// OAuth 1.0a signed token request for HERE platform
// ---------------------------------------------------------------------------

/// Percent-encode a string per RFC 3986 (unreserved characters only).
fn percent_encode(input: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

    const OAUTH_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');

    utf8_percent_encode(input, OAUTH_ENCODE_SET).to_string()
}

/// Build the OAuth 1.0a `Authorization` header for the HERE token endpoint.
fn build_oauth_header(
    consumer_key: &str,
    consumer_secret: &str,
    url: &str,
    timestamp: &str,
    nonce: &str,
) -> String {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    // Sorted parameters (body param + oauth_ params)
    let mut params: Vec<(&str, &str)> = vec![
        ("grant_type", "client_credentials"),
        ("oauth_consumer_key", consumer_key),
        ("oauth_nonce", nonce),
        ("oauth_signature_method", "HMAC-SHA256"),
        ("oauth_timestamp", timestamp),
        ("oauth_version", "1.0"),
    ];
    params.sort_by_key(|&(k, _)| k);

    let param_string: String = params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    // Signature base string: METHOD&url&params
    let base_string = format!(
        "POST&{}&{}",
        percent_encode(url),
        percent_encode(&param_string)
    );

    // Signing key: consumer_secret& (empty token secret)
    let signing_key = format!("{}&", percent_encode(consumer_secret));

    type HmacSha256 = Hmac<Sha256>;
    let mut mac =
        HmacSha256::new_from_slice(signing_key.as_bytes()).expect("HMAC accepts any key size");
    mac.update(base_string.as_bytes());
    let signature = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    format!(
        r#"OAuth oauth_consumer_key="{}",oauth_nonce="{}",oauth_signature="{}",oauth_signature_method="HMAC-SHA256",oauth_timestamp="{}",oauth_version="1.0""#,
        percent_encode(consumer_key),
        percent_encode(nonce),
        percent_encode(&signature),
        percent_encode(timestamp),
    )
}

/// Exchange OAuth credentials for a bearer token.
async fn fetch_oauth_token(
    client: &reqwest::Client,
    access_key_id: &str,
    access_key_secret: &str,
    token_url: &str,
) -> Result<TokenResponse> {
    let timestamp = chrono::Utc::now().timestamp().to_string();
    let nonce = uuid::Uuid::new_v4().to_string();

    let auth_header = build_oauth_header(
        access_key_id,
        access_key_secret,
        token_url,
        &timestamp,
        &nonce,
    );

    let response = client
        .post(token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Authorization", &auth_header)
        .body("grant_type=client_credentials")
        .send()
        .await?;

    let status = response.status();
    let text = response.text().await?;

    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "HERE OAuth token request failed ({status}): {text}"
        ));
    }

    let token: TokenResponse = serde_json::from_str(&text)?;
    Ok(token)
}
