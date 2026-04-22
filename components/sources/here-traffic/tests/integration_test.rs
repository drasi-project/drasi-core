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

//! Integration test for HERE Traffic source using a mock HTTP server.

use anyhow::{Context, Result};
use drasi_bootstrap_here_traffic::HereTrafficBootstrapProvider;
use drasi_lib::channels::ResultDiff;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::subscription::SubscriptionOptions;
use drasi_reaction_application::ApplicationReaction;
use drasi_source_here_traffic::{AuthMethod, Endpoint, HereTrafficClient, HereTrafficSource};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use std::time::Instant;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

const SOURCE_ID: &str = "here-traffic-source";
const SEGMENTS_QUERY: &str = "segments-query";
const INCIDENTS_QUERY: &str = "incidents-query";

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn field_matches(data: &Value, field: &str, expected: &str) -> bool {
    data.get(field)
        .and_then(value_as_string)
        .map(|value| value == expected)
        .unwrap_or(false)
}

fn matches_fields(data: &Value, fields: &[(&str, &str)]) -> bool {
    fields
        .iter()
        .all(|(field, expected)| field_matches(data, field, expected))
}

fn matches_change(entry: &ResultDiff, change_type: &str, fields: &[(&str, &str)]) -> bool {
    match (change_type, entry) {
        ("ADD", ResultDiff::Add { data })
        | ("DELETE", ResultDiff::Delete { data })
        | ("UPDATE", ResultDiff::Update { data, .. }) => matches_fields(data, fields),
        _ => false,
    }
}

fn matches_update(
    entry: &ResultDiff,
    before_fields: &[(&str, &str)],
    after_fields: &[(&str, &str)],
) -> bool {
    match entry {
        ResultDiff::Update { before, after, .. } => {
            matches_fields(before, before_fields) && matches_fields(after, after_fields)
        }
        _ => false,
    }
}

async fn wait_for_change<F>(
    subscription: &mut drasi_reaction_application::subscription::Subscription,
    attempts: usize,
    mut matcher: F,
) -> Result<ResultDiff>
where
    F: FnMut(&ResultDiff) -> bool,
{
    let mut last_results: Option<Vec<ResultDiff>> = None;
    for _ in 0..attempts {
        let result = tokio::time::timeout(Duration::from_secs(5), subscription.recv()).await;
        if let Ok(Some(result)) = result {
            last_results = Some(result.results.clone());
            for entry in &result.results {
                if matcher(entry) {
                    return Ok(entry.clone());
                }
            }
        }
    }

    anyhow::bail!("Timed out waiting for expected change event. Last results: {last_results:?}");
}

fn flow_body(speed: f64, jam_factor: f64) -> Value {
    json!({
        "results": [{
            "location": {
                "description": "Test Street",
                "length": 1500.0,
                "shape": {
                    "links": [{
                        "points": [
                            { "lat": 52.5, "lng": 13.4 }
                        ]
                    }]
                }
            },
            "currentFlow": {
                "speed": speed,
                "freeFlow": 13.9,
                "jamFactor": jam_factor,
                "confidence": 0.9
            }
        }]
    })
}

fn empty_flow_body() -> Value {
    json!({
        "results": []
    })
}

struct RateLimitResponder {
    counter: Arc<AtomicUsize>,
    flow_body: Value,
}

impl Respond for RateLimitResponder {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        let attempt = self.counter.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            ResponseTemplate::new(429).insert_header("Retry-After", "1")
        } else {
            ResponseTemplate::new(200).set_body_json(self.flow_body.clone())
        }
    }
}

/// Convert user-format bbox (lat1,lon1,lat2,lon2) to HERE API format (west,south,east,north).
fn to_here_bbox(bbox: &str) -> String {
    let parts: Vec<f64> = bbox
        .split(',')
        .map(|s| s.trim().parse().expect("invalid bbox coordinate"))
        .collect();
    let (lat1, lon1, lat2, lon2) = (parts[0], parts[1], parts[2], parts[3]);
    let west = lon1.min(lon2);
    let south = lat1.min(lat2);
    let east = lon1.max(lon2);
    let north = lat1.max(lat2);
    format!("{west},{south},{east},{north}")
}

async fn mount_flow_mock(
    server: &MockServer,
    bbox: &str,
    api_key: &str,
    speed: f64,
    jam_factor: f64,
) {
    let flow_body = flow_body(speed, jam_factor);
    let here_bbox = to_here_bbox(bbox);

    Mock::given(method("GET"))
        .and(path("/v7/flow"))
        .and(query_param("in", format!("bbox:{here_bbox}")))
        .and(query_param("apiKey", api_key))
        .respond_with(ResponseTemplate::new(200).set_body_json(flow_body))
        .mount(server)
        .await;
}

async fn mount_empty_flow_mock(server: &MockServer, bbox: &str, api_key: &str) {
    let flow_body = empty_flow_body();
    let here_bbox = to_here_bbox(bbox);
    Mock::given(method("GET"))
        .and(path("/v7/flow"))
        .and(query_param("in", format!("bbox:{here_bbox}")))
        .and(query_param("apiKey", api_key))
        .respond_with(ResponseTemplate::new(200).set_body_json(flow_body))
        .mount(server)
        .await;
}

async fn mount_incident_mock(
    server: &MockServer,
    bbox: &str,
    api_key: &str,
    include_incident: bool,
) {
    let incidents_body = if include_incident {
        json!({
            "results": [{
                "incidentDetails": {
                    "id": "INC_001",
                    "type": "ACCIDENT",
                    "severity": 3,
                    "summary": { "value": "Test incident" },
                    "startTime": "2024-03-10T11:00:00Z"
                },
                "location": {
                    "shape": {
                        "links": [{
                            "points": [
                                { "lat": 52.5005, "lng": 13.4005 }
                            ]
                        }]
                    }
                }
            }]
        })
    } else {
        json!({ "results": [] })
    };
    let here_bbox = to_here_bbox(bbox);

    Mock::given(method("GET"))
        .and(path("/v7/incidents"))
        .and(query_param("in", format!("bbox:{here_bbox}")))
        .and(query_param("apiKey", api_key))
        .respond_with(ResponseTemplate::new(200).set_body_json(incidents_body))
        .mount(server)
        .await;
}

#[tokio::test]
#[ignore]
async fn test_here_traffic_change_detection_end_to_end() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let mock_server = MockServer::start().await;
    let bbox = "52.5,13.3,52.6,13.5";
    let api_key = "test_key";

    mount_empty_flow_mock(&mock_server, bbox, api_key).await;
    mount_incident_mock(&mock_server, bbox, api_key, false).await;

    let bootstrap_provider = HereTrafficBootstrapProvider::builder()
        .with_source_id(SOURCE_ID)
        .with_api_key(api_key)
        .with_bounding_box(bbox)
        .with_endpoints(vec![Endpoint::Flow, Endpoint::Incidents])
        .with_base_url(mock_server.uri())
        .build()?;

    let source = HereTrafficSource::builder(SOURCE_ID, api_key, bbox)
        .with_polling_interval(Duration::from_secs(1))
        .with_base_url(mock_server.uri())
        .with_bootstrap_provider(bootstrap_provider)
        .build()?;

    let segments_query = Query::cypher(SEGMENTS_QUERY)
        .query(
            r#"
            MATCH (s:TrafficSegment)
            RETURN s.id AS id, s.jam_factor AS jam_factor
            "#,
        )
        .from_source(SOURCE_ID)
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let incidents_query = Query::cypher(INCIDENTS_QUERY)
        .query(
            r#"
            MATCH (i:TrafficIncident)
            RETURN i.id AS id, i.status AS status
            "#,
        )
        .from_source(SOURCE_ID)
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let (segments_reaction, segments_handle) = ApplicationReaction::builder("segments-reaction")
        .with_query(SEGMENTS_QUERY)
        .build();

    let (incidents_reaction, incidents_handle) = ApplicationReaction::builder("incidents-reaction")
        .with_query(INCIDENTS_QUERY)
        .build();

    let core = DrasiLib::builder()
        .with_id("here-traffic-integration-test")
        .with_source(source)
        .with_query(segments_query)
        .with_query(incidents_query)
        .with_reaction(segments_reaction)
        .with_reaction(incidents_reaction)
        .build()
        .await?;

    let options = SubscriptionOptions::default().with_timeout(Duration::from_secs(10));
    let mut segments_subscription = segments_handle
        .subscribe_with_options(options.clone())
        .await?;
    let mut incidents_subscription = incidents_handle
        .subscribe_with_options(options.clone())
        .await?;

    core.start().await?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    mock_server.reset().await;
    mount_flow_mock(&mock_server, bbox, api_key, 45.0, 3.5).await;
    mount_incident_mock(&mock_server, bbox, api_key, true).await;

    wait_for_change(&mut segments_subscription, 5, |entry| {
        matches_change(entry, "ADD", &[("id", "segment_52.50000_13.40000")])
    })
    .await
    .context("Did not observe segment ADD")?;

    wait_for_change(&mut incidents_subscription, 5, |entry| {
        matches_change(entry, "ADD", &[("id", "INC_001")])
    })
    .await
    .context("Did not observe incident ADD")?;

    mock_server.reset().await;
    mount_flow_mock(&mock_server, bbox, api_key, 25.0, 7.5).await;
    mount_incident_mock(&mock_server, bbox, api_key, false).await;

    wait_for_change(&mut segments_subscription, 5, |entry| {
        matches_update(
            entry,
            &[("id", "segment_52.50000_13.40000")],
            &[("id", "segment_52.50000_13.40000")],
        ) || matches_change(entry, "UPDATE", &[("id", "segment_52.50000_13.40000")])
    })
    .await
    .context("Did not observe segment UPDATE")?;

    wait_for_change(&mut incidents_subscription, 5, |entry| {
        matches_change(entry, "DELETE", &[("id", "INC_001")])
    })
    .await
    .context("Did not observe incident DELETE")?;

    core.stop().await?;

    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_rate_limit_backoff() -> Result<()> {
    let mock_server = MockServer::start().await;
    let bbox = "52.5,13.3,52.6,13.5";
    let api_key = "test_key";

    let call_counter = Arc::new(AtomicUsize::new(0));
    let flow_body = flow_body(45.0, 3.5);
    let responder = RateLimitResponder {
        counter: Arc::clone(&call_counter),
        flow_body,
    };
    let here_bbox = to_here_bbox(bbox);
    Mock::given(method("GET"))
        .and(path("/v7/flow"))
        .and(query_param("in", format!("bbox:{here_bbox}")))
        .and(query_param("apiKey", api_key))
        .respond_with(responder)
        .expect(2)
        .mount(&mock_server)
        .await;

    let client = HereTrafficClient::new(
        AuthMethod::ApiKey {
            api_key: api_key.to_string(),
        },
        mock_server.uri(),
    )?;
    let start = Instant::now();
    client.get_flow(bbox).await?;
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_secs(1),
        "Expected backoff to wait at least 1s, waited {elapsed:?}"
    );

    Ok(())
}
