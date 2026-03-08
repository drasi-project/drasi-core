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

use anyhow::{anyhow, Context, Result};
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use drasi_bootstrap_gtfs_rt::{GtfsRtBootstrapConfig, GtfsRtBootstrapProvider};
use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::{DrasiLib, Query, QueryConfig};
use drasi_reaction_application::{subscription::SubscriptionOptions, ApplicationReaction};
use drasi_source_gtfs_rt::GtfsRtSource;
use futures_util::StreamExt;
use log::{error, info, warn};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::convert::Infallible;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};
use tokio_stream::wrappers::BroadcastStream;
use tower_http::services::ServeDir;

const QUERY_VEHICLES: &str = "dashboard-vehicle-positions";
const QUERY_DELAYS: &str = "dashboard-trip-delays";
const QUERY_ALERTS: &str = "dashboard-active-alerts";

#[derive(Debug, Clone)]
struct ExampleConfig {
    trip_updates_url: Option<String>,
    vehicle_positions_url: Option<String>,
    alerts_url: Option<String>,
    poll_interval_secs: u64,
    timeout_secs: u64,
    language: String,
    dashboard_host: String,
    dashboard_port: u16,
}

impl ExampleConfig {
    fn from_env() -> Result<Self> {
        let trip_updates_url = env_feed(
            "GTFS_RT_TRIP_UPDATES_URL",
            Some("https://www.rtd-denver.com/files/gtfs-rt/TripUpdate.pb"),
        );
        let vehicle_positions_url = env_feed(
            "GTFS_RT_VEHICLE_POSITIONS_URL",
            Some("https://www.rtd-denver.com/files/gtfs-rt/VehiclePosition.pb"),
        );
        let alerts_url = env_feed(
            "GTFS_RT_ALERTS_URL",
            Some("https://www.rtd-denver.com/files/gtfs-rt/Alerts.pb"),
        );

        if trip_updates_url.is_none() && vehicle_positions_url.is_none() && alerts_url.is_none() {
            return Err(anyhow!(
                "at least one feed must be configured (GTFS_RT_TRIP_UPDATES_URL, GTFS_RT_VEHICLE_POSITIONS_URL, GTFS_RT_ALERTS_URL)"
            ));
        }

        let poll_interval_secs = env::var("GTFS_RT_POLL_INTERVAL_SECS")
            .ok()
            .map(|v| v.parse::<u64>())
            .transpose()
            .context("GTFS_RT_POLL_INTERVAL_SECS must be an integer")?
            .unwrap_or(30)
            .max(1);

        let timeout_secs = env::var("GTFS_RT_TIMEOUT_SECS")
            .ok()
            .map(|v| v.parse::<u64>())
            .transpose()
            .context("GTFS_RT_TIMEOUT_SECS must be an integer")?
            .unwrap_or(15)
            .max(1);

        let dashboard_port = env::var("GTFS_RT_DASHBOARD_PORT")
            .ok()
            .map(|v| v.parse::<u16>())
            .transpose()
            .context("GTFS_RT_DASHBOARD_PORT must be an integer")?
            .unwrap_or(8090);

        Ok(Self {
            trip_updates_url,
            vehicle_positions_url,
            alerts_url,
            poll_interval_secs,
            timeout_secs,
            language: env::var("GTFS_RT_LANGUAGE").unwrap_or_else(|_| "en".to_string()),
            dashboard_host: env::var("GTFS_RT_DASHBOARD_HOST")
                .unwrap_or_else(|_| "0.0.0.0".to_string()),
            dashboard_port,
        })
    }
}

fn env_feed(name: &str, default: Option<&str>) -> Option<String> {
    match env::var(name) {
        Ok(value) if value.trim().is_empty() => None,
        Ok(value) => Some(value),
        Err(_) => default.map(|v| v.to_string()),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct VehicleMarker {
    entity_id: String,
    vehicle_id: String,
    vehicle_label: Option<String>,
    route_id: Option<String>,
    trip_id: Option<String>,
    latitude: f64,
    longitude: f64,
    speed: Option<f64>,
    timestamp: Option<i64>,
    delay_seconds: i64,
    delay_color: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DelayLeaderboardEntry {
    entity_id: String,
    trip_id: Option<String>,
    route_id: Option<String>,
    vehicle_label: Option<String>,
    delay_seconds: i64,
    delay_minutes: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AlertCard {
    alert_id: String,
    severity_level: Option<String>,
    cause: Option<String>,
    effect: Option<String>,
    header_text: Option<String>,
    description_text: Option<String>,
    active_period_start: Option<i64>,
    active_period_end: Option<i64>,
    route_ids: Vec<String>,
    stop_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RouteStat {
    route_id: String,
    vehicle_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DashboardSnapshot {
    generated_at: String,
    sequence: u64,
    vehicles: Vec<VehicleMarker>,
    delay_leaderboard: Vec<DelayLeaderboardEntry>,
    alerts: Vec<AlertCard>,
    route_statistics: Vec<RouteStat>,
}

#[derive(Debug, Default)]
struct DashboardAccumulator {
    positions_rows: Vec<Value>,
    delays_rows: Vec<Value>,
    alerts_rows: Vec<Value>,
    sequence: u64,
}

impl DashboardAccumulator {
    fn new() -> Self {
        Self {
            positions_rows: Vec::new(),
            delays_rows: Vec::new(),
            alerts_rows: Vec::new(),
            sequence: 0,
        }
    }

    fn apply_query_result(&mut self, result: &QueryResult) -> bool {
        let target_rows = match result.query_id.as_str() {
            QUERY_VEHICLES => &mut self.positions_rows,
            QUERY_DELAYS => &mut self.delays_rows,
            QUERY_ALERTS => &mut self.alerts_rows,
            _ => return false,
        };

        let change_count = apply_result_diffs(target_rows, &result.results);
        if change_count > 0 {
            self.sequence = self.sequence.saturating_add(1);
            true
        } else {
            false
        }
    }

    fn snapshot(&self) -> DashboardSnapshot {
        let now = Utc::now();

        let mut delay_by_trip = HashMap::<String, i64>::new();
        for row in &self.delays_rows {
            if let Some(trip_id) = get_string(row, "trip_id") {
                let delay = get_i64(row, "delay").unwrap_or(0);
                delay_by_trip.insert(trip_id, delay);
            }
        }

        let mut vehicle_label_by_trip = HashMap::<String, String>::new();
        for row in &self.positions_rows {
            if let (Some(trip_id), Some(vehicle_label)) =
                (get_string(row, "trip_id"), get_string(row, "vehicle_label"))
            {
                vehicle_label_by_trip
                    .entry(trip_id)
                    .or_insert(vehicle_label);
            }
        }

        let mut vehicles = Vec::new();
        for row in &self.positions_rows {
            let Some(entity_id) = get_string(row, "entity_id") else {
                continue;
            };
            let Some(vehicle_id) = get_string(row, "vehicle_id") else {
                continue;
            };
            let Some(latitude) = get_f64(row, "latitude") else {
                continue;
            };
            let Some(longitude) = get_f64(row, "longitude") else {
                continue;
            };

            let trip_id = get_string(row, "trip_id");
            let delay_seconds = trip_id
                .as_ref()
                .and_then(|trip| delay_by_trip.get(trip))
                .copied()
                .unwrap_or(0);

            let delay_color = if delay_seconds >= 600 {
                "red"
            } else if delay_seconds >= 120 {
                "yellow"
            } else {
                "green"
            }
            .to_string();

            vehicles.push(VehicleMarker {
                entity_id,
                vehicle_id,
                vehicle_label: get_string(row, "vehicle_label"),
                route_id: get_string(row, "route_id"),
                trip_id,
                latitude,
                longitude,
                speed: get_f64(row, "speed"),
                timestamp: get_i64(row, "timestamp"),
                delay_seconds,
                delay_color,
            });
        }

        let mut delay_leaderboard = Vec::new();
        for row in &self.delays_rows {
            let delay_seconds = get_i64(row, "delay").unwrap_or(0);
            if delay_seconds <= 0 {
                continue;
            }

            let trip_id = get_string(row, "trip_id");
            let vehicle_label = trip_id
                .as_ref()
                .and_then(|trip| vehicle_label_by_trip.get(trip))
                .cloned();

            delay_leaderboard.push(DelayLeaderboardEntry {
                entity_id: get_string(row, "entity_id").unwrap_or_else(|| "unknown".to_string()),
                trip_id,
                route_id: get_string(row, "route_id"),
                vehicle_label,
                delay_seconds,
                delay_minutes: delay_seconds as f64 / 60.0,
            });
        }

        delay_leaderboard.sort_by(|a, b| b.delay_seconds.cmp(&a.delay_seconds));
        delay_leaderboard.truncate(25);

        #[derive(Default)]
        struct AlertAggregate {
            severity_level: Option<String>,
            cause: Option<String>,
            effect: Option<String>,
            header_text: Option<String>,
            description_text: Option<String>,
            active_period_start: Option<i64>,
            active_period_end: Option<i64>,
            route_ids: BTreeSet<String>,
            stop_ids: BTreeSet<String>,
        }

        let mut alert_map: BTreeMap<String, AlertAggregate> = BTreeMap::new();
        for row in &self.alerts_rows {
            let Some(alert_id) = get_string(row, "alert_id") else {
                continue;
            };

            let aggregate = alert_map.entry(alert_id).or_default();
            aggregate.severity_level = aggregate
                .severity_level
                .clone()
                .or_else(|| get_string(row, "severity_level"));
            aggregate.cause = aggregate.cause.clone().or_else(|| get_string(row, "cause"));
            aggregate.effect = aggregate
                .effect
                .clone()
                .or_else(|| get_string(row, "effect"));
            aggregate.header_text = aggregate
                .header_text
                .clone()
                .or_else(|| get_string(row, "header_text"));
            aggregate.description_text = aggregate
                .description_text
                .clone()
                .or_else(|| get_string(row, "description_text"));
            aggregate.active_period_start = aggregate
                .active_period_start
                .or_else(|| get_i64(row, "active_period_start"));
            aggregate.active_period_end = aggregate
                .active_period_end
                .or_else(|| get_i64(row, "active_period_end"));

            if let Some(route_id) = get_string(row, "route_id") {
                aggregate.route_ids.insert(route_id);
            }
            if let Some(stop_id) = get_string(row, "stop_id") {
                aggregate.stop_ids.insert(stop_id);
            }
        }

        let mut alerts = alert_map
            .into_iter()
            .map(|(alert_id, aggregate)| AlertCard {
                alert_id,
                severity_level: aggregate.severity_level,
                cause: aggregate.cause,
                effect: aggregate.effect,
                header_text: aggregate.header_text,
                description_text: aggregate.description_text,
                active_period_start: aggregate.active_period_start,
                active_period_end: aggregate.active_period_end,
                route_ids: aggregate.route_ids.into_iter().collect(),
                stop_ids: aggregate.stop_ids.into_iter().collect(),
            })
            .collect::<Vec<_>>();

        alerts.sort_by_key(|alert| severity_rank(alert.severity_level.as_deref()));

        let mut route_count = HashMap::<String, usize>::new();
        for vehicle in &vehicles {
            if let Some(route_id) = vehicle.route_id.clone() {
                *route_count.entry(route_id).or_default() += 1;
            }
        }

        let mut route_statistics = route_count
            .into_iter()
            .map(|(route_id, vehicle_count)| RouteStat {
                route_id,
                vehicle_count,
            })
            .collect::<Vec<_>>();
        route_statistics.sort_by(|a, b| {
            b.vehicle_count
                .cmp(&a.vehicle_count)
                .then_with(|| a.route_id.cmp(&b.route_id))
        });

        DashboardSnapshot {
            generated_at: now.to_rfc3339(),
            sequence: self.sequence,
            vehicles,
            delay_leaderboard,
            alerts,
            route_statistics,
        }
    }
}

fn severity_rank(severity: Option<&str>) -> u8 {
    match severity {
        Some("SEVERE") => 0,
        Some("WARNING") => 1,
        Some("INFO") => 2,
        _ => 3,
    }
}

fn apply_result_diffs(rows: &mut Vec<Value>, diffs: &[ResultDiff]) -> usize {
    let mut changes = 0usize;

    for diff in diffs {
        match diff {
            ResultDiff::Add { data } => {
                rows.push(data.clone());
                changes += 1;
            }
            ResultDiff::Delete { data } => {
                if remove_first(rows, data) {
                    changes += 1;
                }
            }
            ResultDiff::Update { before, after, .. } => {
                if let Some(pos) = rows.iter().position(|row| row == before) {
                    rows[pos] = after.clone();
                } else {
                    let _ = remove_first(rows, before);
                    rows.push(after.clone());
                }
                changes += 1;
            }
            ResultDiff::Aggregation { before, after } => {
                if let Some(before) = before {
                    let _ = remove_first(rows, before);
                }
                rows.push(after.clone());
                changes += 1;
            }
            ResultDiff::Noop => {}
        }
    }

    changes
}

fn remove_first(rows: &mut Vec<Value>, target: &Value) -> bool {
    if let Some(pos) = rows.iter().position(|row| row == target) {
        rows.remove(pos);
        true
    } else {
        false
    }
}

fn get_string(row: &Value, key: &str) -> Option<String> {
    row.get(key).and_then(|v| v.as_str()).map(ToOwned::to_owned)
}

fn get_i64(row: &Value, key: &str) -> Option<i64> {
    row.get(key).and_then(|v| {
        if let Some(i) = v.as_i64() {
            Some(i)
        } else {
            v.as_f64().map(|f| f as i64)
        }
    })
}

fn get_f64(row: &Value, key: &str) -> Option<f64> {
    row.get(key).and_then(|v| {
        if let Some(f) = v.as_f64() {
            Some(f)
        } else {
            v.as_i64().map(|i| i as f64)
        }
    })
}

struct SharedDashboardState {
    accumulator: RwLock<DashboardAccumulator>,
    event_tx: broadcast::Sender<String>,
}

async fn broadcast_snapshot(state: &Arc<SharedDashboardState>) {
    let snapshot = { state.accumulator.read().await.snapshot() };

    match serde_json::to_string(&snapshot) {
        Ok(payload) => {
            let _ = state.event_tx.send(payload);
        }
        Err(err) => {
            error!("Failed to serialize dashboard snapshot: {err}");
        }
    }
}

async fn run_query_result_consumer(
    state: Arc<SharedDashboardState>,
    mut subscription: drasi_reaction_application::subscription::Subscription,
) {
    while let Some(result) = subscription.recv().await {
        let changed = {
            let mut accumulator = state.accumulator.write().await;
            accumulator.apply_query_result(&result)
        };

        if changed {
            broadcast_snapshot(&state).await;
        }
    }
}

async fn get_dashboard_state(
    State(state): State<Arc<SharedDashboardState>>,
) -> Json<DashboardSnapshot> {
    let snapshot = state.accumulator.read().await.snapshot();
    Json(snapshot)
}

async fn get_dashboard_events(
    State(state): State<Arc<SharedDashboardState>>,
) -> Sse<impl futures_util::stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(state.event_tx.subscribe()).filter_map(|message| async {
        match message {
            Ok(payload) => Some(Ok(Event::default().event("dashboard").data(payload))),
            Err(err) => {
                warn!("Dashboard SSE dropped event: {err}");
                None
            }
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    )
}

async fn get_health() -> Json<Value> {
    Json(serde_json::json!({
        "status": "ok",
        "timestamp": Utc::now().to_rfc3339(),
    }))
}

fn build_source(config: &ExampleConfig) -> Result<GtfsRtSource> {
    let bootstrap_config = GtfsRtBootstrapConfig {
        trip_updates_url: config.trip_updates_url.clone(),
        vehicle_positions_url: config.vehicle_positions_url.clone(),
        alerts_url: config.alerts_url.clone(),
        headers: HashMap::new(),
        timeout_secs: config.timeout_secs,
        language: config.language.clone(),
    };

    let bootstrap_provider = GtfsRtBootstrapProvider::builder()
        .with_config(bootstrap_config)
        .build()?;

    let mut builder = GtfsRtSource::builder("gtfs-rt-source")
        .with_poll_interval_secs(config.poll_interval_secs)
        .with_timeout_secs(config.timeout_secs)
        .with_language(config.language.clone())
        .with_bootstrap_provider(bootstrap_provider);

    if let Some(url) = config.trip_updates_url.clone() {
        builder = builder.with_trip_updates_url(url);
    }
    if let Some(url) = config.vehicle_positions_url.clone() {
        builder = builder.with_vehicle_positions_url(url);
    }
    if let Some(url) = config.alerts_url.clone() {
        builder = builder.with_alerts_url(url);
    }

    builder.build()
}

fn build_queries() -> [QueryConfig; 3] {
    let vehicle_positions = Query::cypher(QUERY_VEHICLES)
        .query(
            r#"
            MATCH (v:Vehicle)-[:HAS_POSITION]->(vp:VehiclePosition)
            RETURN vp.entity_id AS entity_id,
                   v.vehicle_id AS vehicle_id,
                   v.label AS vehicle_label,
                   vp.route_id AS route_id,
                   vp.trip_id AS trip_id,
                   vp.latitude AS latitude,
                   vp.longitude AS longitude,
                   vp.speed AS speed,
                   vp.timestamp AS timestamp
            "#,
        )
        .from_source("gtfs-rt-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let trip_delays = Query::cypher(QUERY_DELAYS)
        .query(
            r#"
            MATCH (t:Trip)-[:HAS_UPDATE]->(tu:TripUpdate)
            RETURN tu.entity_id AS entity_id,
                   t.trip_id AS trip_id,
                   t.route_id AS route_id,
                   tu.delay AS delay,
                   tu.timestamp AS timestamp
            "#,
        )
        .from_source("gtfs-rt-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let active_alerts = Query::cypher(QUERY_ALERTS)
        .query(
            r#"
            MATCH (a:Alert)
            OPTIONAL MATCH (a)-[:AFFECTS_ROUTE]->(r:Route)
            OPTIONAL MATCH (a)-[:AFFECTS_STOP]->(s:Stop)
            RETURN a.entity_id AS alert_id,
                   a.severity_level AS severity_level,
                   a.cause AS cause,
                   a.effect AS effect,
                   a.header_text AS header_text,
                   a.description_text AS description_text,
                   a.active_period_start AS active_period_start,
                   a.active_period_end AS active_period_end,
                   r.route_id AS route_id,
                   s.stop_id AS stop_id
            "#,
        )
        .from_source("gtfs-rt-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    [vehicle_positions, trip_delays, active_alerts]
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config = ExampleConfig::from_env()?;

    info!("Starting GTFS-RT dashboard example");
    info!("Trip updates feed: {:?}", config.trip_updates_url);
    info!("Vehicle positions feed: {:?}", config.vehicle_positions_url);
    info!("Alerts feed: {:?}", config.alerts_url);

    let source = build_source(&config)?;
    let [vehicle_positions_query, trip_delays_query, active_alerts_query] = build_queries();

    let (reaction, reaction_handle) = ApplicationReaction::builder("dashboard-reaction")
        .with_query(QUERY_VEHICLES)
        .with_query(QUERY_DELAYS)
        .with_query(QUERY_ALERTS)
        .build();

    let subscription = reaction_handle
        .subscribe_with_options(SubscriptionOptions::default())
        .await?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("gtfs-rt-dashboard")
            .with_source(source)
            .with_query(vehicle_positions_query)
            .with_query(trip_delays_query)
            .with_query(active_alerts_query)
            .with_reaction(reaction)
            .build()
            .await?,
    );

    let (event_tx, _) = broadcast::channel::<String>(256);
    let dashboard_state = Arc::new(SharedDashboardState {
        accumulator: RwLock::new(DashboardAccumulator::new()),
        event_tx,
    });

    let state_for_consumer = dashboard_state.clone();
    let consumer_task = tokio::spawn(async move {
        run_query_result_consumer(state_for_consumer, subscription).await;
    });

    core.start().await?;
    broadcast_snapshot(&dashboard_state).await;

    let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    let app = Router::new()
        .route("/api/dashboard/state", get(get_dashboard_state))
        .route("/api/dashboard/events", get(get_dashboard_events))
        .route("/api/health", get(get_health))
        .fallback_service(ServeDir::new(static_dir).append_index_html_on_directories(true))
        .with_state(dashboard_state.clone());

    let bind_addr = format!("{}:{}", config.dashboard_host, config.dashboard_port);
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("Failed to bind dashboard server to {bind_addr}"))?;

    info!("Dashboard UI: http://{bind_addr}");
    info!("Dashboard API: http://{bind_addr}/api/dashboard/state");
    info!("Press Ctrl+C to stop");

    let server_handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            error!("Dashboard server failed: {err}");
        }
    });

    tokio::signal::ctrl_c().await?;

    info!("Shutting down dashboard example");
    server_handle.abort();
    consumer_task.abort();
    core.stop().await?;

    Ok(())
}
