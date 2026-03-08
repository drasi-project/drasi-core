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

use anyhow::Result;
use axum::{extract::State, routing::get, Router};
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_gtfs_rt::GtfsRtSource;
use gtfs_realtime::{
    FeedEntity, FeedHeader, FeedMessage, Position, VehicleDescriptor, VehiclePosition,
};
use prost::Message;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

fn init_logging() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();
}

fn make_feed(vehicles: &[(&str, f32, f32)]) -> FeedMessage {
    FeedMessage {
        header: FeedHeader {
            gtfs_realtime_version: "2.0".to_string(),
            incrementality: None,
            timestamp: Some(1_720_000_000),
            feed_version: None,
        },
        entity: vehicles
            .iter()
            .enumerate()
            .map(|(index, (vehicle_id, lat, lon))| FeedEntity {
                id: format!("entity-{}", index + 1),
                is_deleted: Some(false),
                trip_update: None,
                vehicle: Some(VehiclePosition {
                    trip: None,
                    vehicle: Some(VehicleDescriptor {
                        id: Some((*vehicle_id).to_string()),
                        label: Some(format!("Vehicle {vehicle_id}")),
                        license_plate: None,
                        wheelchair_accessible: None,
                    }),
                    position: Some(Position {
                        latitude: *lat,
                        longitude: *lon,
                        bearing: None,
                        odometer: None,
                        speed: None,
                    }),
                    current_stop_sequence: None,
                    stop_id: None,
                    current_status: None,
                    timestamp: Some(1_720_000_010),
                    congestion_level: None,
                    occupancy_status: None,
                    occupancy_percentage: None,
                    multi_carriage_details: vec![],
                }),
                alert: None,
                shape: None,
                stop: None,
                trip_modifications: None,
            })
            .collect(),
    }
}

async fn serve_feed(State(feed): State<Arc<RwLock<FeedMessage>>>) -> Vec<u8> {
    feed.read().await.clone().encode_to_vec()
}

async fn wait_for_query_results(
    core: &Arc<DrasiLib>,
    query_id: &str,
    predicate: impl Fn(&[serde_json::Value]) -> bool,
) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(20);

    loop {
        match core.get_query_results(query_id).await {
            Ok(results) => {
                if predicate(&results) {
                    return Ok(());
                }
            }
            Err(err) => {
                // Query startup and subscription happen asynchronously right after core.start().
                // Keep waiting until it transitions to running instead of failing immediately.
                if !err.to_string().contains("is not running") {
                    return Err(err.into());
                }
            }
        }
        if start.elapsed() > timeout {
            anyhow::bail!("Timed out waiting for query '{query_id}'");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn has_exact_entity_ids(rows: &[Value], expected: &[&str]) -> bool {
    if rows.len() != expected.len() {
        return false;
    }

    let actual: std::collections::BTreeSet<String> = rows
        .iter()
        .filter_map(|row| row.get("entity_id").and_then(|value| value.as_str()))
        .map(ToOwned::to_owned)
        .collect();
    let expected: std::collections::BTreeSet<String> =
        expected.iter().map(|value| (*value).to_string()).collect();

    actual == expected
}

#[tokio::test]
#[ignore]
async fn test_gtfs_rt_source_detects_insert_update_delete() -> Result<()> {
    init_logging();
    let run_suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let source_id = format!("gtfs-test-source-{run_suffix}");
    let query_id = format!("vp-query-{run_suffix}");
    let reaction_id = format!("test-reaction-{run_suffix}");
    let core_id = format!("gtfs-test-core-{run_suffix}");

    let feed_state = Arc::new(RwLock::new(make_feed(&[
        ("veh-1", 39.70, -104.99),
        ("veh-2", 39.71, -104.98),
    ])));

    let app = Router::new()
        .route("/vehicle_positions.pb", get(serve_feed))
        .with_state(feed_state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let source = GtfsRtSource::builder(source_id.clone())
        .with_vehicle_positions_url(format!("http://{addr}/vehicle_positions.pb"))
        .with_poll_interval_secs(1)
        .build()?;

    let query = Query::cypher(query_id.clone())
        .query(
            r#"
            MATCH (vp:VehiclePosition)
            RETURN vp.entity_id AS entity_id, vp.vehicle_id AS vehicle_id, vp.latitude AS latitude
            "#,
        )
        .from_source(source_id.clone())
        .auto_start(true)
        .enable_bootstrap(false)
        .build();

    let (reaction, _handle) = ApplicationReaction::builder(reaction_id)
        .with_query(query_id.clone())
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id(core_id)
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await?,
    );

    core.start().await?;

    wait_for_query_results(&core, &query_id, |rows| {
        has_exact_entity_ids(rows, &["entity-1", "entity-2"])
    })
    .await?;

    // Update one vehicle, add one vehicle.
    {
        let mut feed = feed_state.write().await;
        *feed = make_feed(&[
            ("veh-1", 39.75, -104.95),
            ("veh-2", 39.71, -104.98),
            ("veh-3", 39.72, -104.97),
        ]);
    }

    wait_for_query_results(&core, &query_id, |rows| {
        has_exact_entity_ids(rows, &["entity-1", "entity-2", "entity-3"])
    })
    .await?;

    // Phase 3: Feed now has 2 vehicles — veh-1 stays, veh-2 is gone, veh-3 is new.
    // Because make_feed assigns entity IDs by index, entity-2 switches from veh-2
    // to veh-3 (update) and entity-3 is removed (delete).
    {
        let mut feed = feed_state.write().await;
        *feed = make_feed(&[("veh-1", 39.75, -104.95), ("veh-3", 39.72, -104.97)]);
    }

    wait_for_query_results(&core, &query_id, |rows| {
        has_exact_entity_ids(rows, &["entity-1", "entity-2"])
    })
    .await?;

    core.stop().await?;
    server.abort();

    Ok(())
}
