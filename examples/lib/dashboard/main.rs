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

//! # DrasiLib Dashboard Example — IoT Sensor Monitor
//!
//! A self-contained demo that auto-generates live IoT sensor data and renders
//! it on a visual dashboard.  No external data sources or HTTP endpoints are
//! required — just `cargo run` and open the browser.
//!
//! ## Running
//!
//! ```bash
//! cargo run
//! ```
//!
//! Then open <http://localhost:3000> in your browser.
//!
//! ## Architecture
//!
//! ```text
//! MockSource (10 sensors, 2 s interval)
//!   └─► 4 Cypher Queries ──► Dashboard Reaction (port 3000)
//! ```
//!
//! The MockSource generates random temperature (20–30 °C) and humidity (40–60 %)
//! readings for 10 virtual sensors every 2 seconds.  Four continuous queries
//! slice the data in different ways and feed a predefined dashboard with six
//! widgets — including a markdown widget that uses `{{#if}}` conditionals to
//! render status emojis.

use anyhow::Result;
use std::sync::Arc;

use drasi_lib::{DrasiLib, Query};
use drasi_reaction_dashboard::{
    DashboardConfig, DashboardReaction, DashboardWidget, GridOptions, WidgetGrid,
};
use drasi_source_mock::{DataType, MockSource};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("╔════════════════════════════════════════════╗");
    println!("║   DrasiLib IoT Sensor Dashboard Example    ║");
    println!("╚════════════════════════════════════════════╝\n");

    // =========================================================================
    // Step 1: Create Mock Source
    // =========================================================================
    // Generates random temperature + humidity readings for 10 virtual sensors
    // every 2 seconds.  First reading per sensor → INSERT, subsequent → UPDATE.

    let mock_source = MockSource::builder("sensors")
        .with_data_type(DataType::sensor_reading(10))
        .with_interval_ms(2000)
        .build()?;

    // =========================================================================
    // Step 2: Define Queries
    // =========================================================================
    // Four queries over the same source, each feeding a different widget type.

    // Query 1: All Sensors — full table of current readings
    let all_sensors = Query::cypher("all-sensors")
        .query(
            r#"
            MATCH (s:SensorReading)
            RETURN s.sensor_id AS sensor_id,
                   s.temperature AS temperature,
                   s.humidity AS humidity,
                   s.timestamp AS timestamp
        "#,
        )
        .from_source("sensors")
        .auto_start(true)
        .build();

    // Query 2: Hot Sensors — those above 27 °C (for KPI / alerts)
    let hot_sensors = Query::cypher("hot-sensors")
        .query(
            r#"
            MATCH (s:SensorReading)
            WHERE s.temperature > 27
            RETURN s.sensor_id AS sensor_id,
                   s.temperature AS temperature
        "#,
        )
        .from_source("sensors")
        .auto_start(true)
        .build();

    // Query 3: Humid Sensors — humidity above 50 % (for KPI / alerts)
    let humid_sensors = Query::cypher("humid-sensors")
        .query(
            r#"
            MATCH (s:SensorReading)
            WHERE s.humidity > 50
            RETURN s.sensor_id AS sensor_id,
                   s.humidity AS humidity
        "#,
        )
        .from_source("sensors")
        .auto_start(true)
        .build();

    // Query 4: Sensor Overview — all sensors for aggregate stats (Gauge / Text)
    let sensor_overview = Query::cypher("sensor-overview")
        .query(
            r#"
            MATCH (s:SensorReading)
            RETURN s.sensor_id AS sensor_id,
                   s.temperature AS temperature,
                   s.humidity AS humidity
        "#,
        )
        .from_source("sensors")
        .auto_start(true)
        .build();

    // =========================================================================
    // Step 3: Create Dashboard Reaction with a predefined dashboard
    // =========================================================================

    let predefined_dashboard = DashboardConfig::with_id(
        "iot-monitor",
        "IoT Sensor Monitor".to_string(),
        GridOptions::default(),
        vec![
            // ── Table: All sensor readings ──────────────────────────────
            DashboardWidget {
                id: "w-table-all".to_string(),
                widget_type: "table".to_string(),
                title: "All Sensors".to_string(),
                grid: WidgetGrid { x: 0, y: 0, w: 8, h: 4 },
                config: serde_json::json!({
                    "queryId": "all-sensors",
                    "columns": ["sensor_id", "temperature", "humidity", "timestamp"]
                }),
            },
            // ── KPI: Total sensor count ─────────────────────────────────
            DashboardWidget {
                id: "w-kpi-total".to_string(),
                widget_type: "kpi".to_string(),
                title: "Sensors Online".to_string(),
                grid: WidgetGrid { x: 8, y: 0, w: 4, h: 2 },
                config: serde_json::json!({
                    "queryId": "all-sensors",
                    "valueField": "sensor_id",
                    "aggregation": "count",
                    "label": "Sensors Online"
                }),
            },
            // ── KPI: Hot sensor alert count ──────────────────────────────
            DashboardWidget {
                id: "w-kpi-hot".to_string(),
                widget_type: "kpi".to_string(),
                title: "🔥 Hot Alerts".to_string(),
                grid: WidgetGrid { x: 8, y: 2, w: 4, h: 2 },
                config: serde_json::json!({
                    "queryId": "hot-sensors",
                    "valueField": "sensor_id",
                    "aggregation": "count",
                    "label": "Hot Alerts"
                }),
            },
            // ── Bar Chart: Temperature by sensor ────────────────────────
            DashboardWidget {
                id: "w-bar-temp".to_string(),
                widget_type: "bar_chart".to_string(),
                title: "Temperature by Sensor".to_string(),
                grid: WidgetGrid { x: 0, y: 4, w: 6, h: 4 },
                config: serde_json::json!({
                    "queryId": "sensor-overview",
                    "categoryField": "sensor_id",
                    "valueFields": ["temperature"]
                }),
            },
            // ── Gauge: Max temperature ──────────────────────────────────
            DashboardWidget {
                id: "w-gauge-max".to_string(),
                widget_type: "gauge".to_string(),
                title: "Max Temperature".to_string(),
                grid: WidgetGrid { x: 6, y: 4, w: 3, h: 4 },
                config: serde_json::json!({
                    "queryId": "sensor-overview",
                    "valueField": "temperature",
                    "aggregation": "max",
                    "min": 15,
                    "max": 35
                }),
            },
            // ── Markdown: Environment status with emoji if-blocks ───────
            DashboardWidget {
                id: "w-md-status".to_string(),
                widget_type: "text".to_string(),
                title: "Environment Status".to_string(),
                grid: WidgetGrid { x: 9, y: 4, w: 3, h: 4 },
                config: serde_json::json!({
                    "queryId": "sensor-overview",
                    "template": concat!(
                        "## 🌡️ Environment\n\n",
                        "{{#if (gt (avg \"temperature\") 27)}}",
                        "🔥 Avg temp is **HIGH**\n\n",
                        "{{else if (lt (avg \"temperature\") 22)}}",
                        "❄️ Avg temp is **LOW**\n\n",
                        "{{else}}",
                        "✅ Temp is **normal**\n\n",
                        "{{/if}}",
                        "{{#if (gt (avg \"humidity\") 55)}}",
                        "💧 Humidity is **HIGH**\n\n",
                        "{{else if (lt (avg \"humidity\") 42)}}",
                        "🏜️ Humidity is **LOW**\n\n",
                        "{{else}}",
                        "✅ Humidity is **normal**\n\n",
                        "{{/if}}",
                        "| Metric | Value |\n",
                        "|--------|-------|\n",
                        "| Sensors | {{count}} |\n",
                        "| Avg Temp | {{avg \"temperature\"}} °C |\n",
                        "| Max Temp | {{max \"temperature\"}} °C |\n",
                        "| Avg Humidity | {{avg \"humidity\"}} % |\n",
                    )
                }),
            },
        ],
    );

    let dashboard_reaction = DashboardReaction::builder("sensor-dashboard")
        .with_query("all-sensors")
        .with_query("hot-sensors")
        .with_query("humid-sensors")
        .with_query("sensor-overview")
        .with_host("0.0.0.0")
        .with_port(3000)
        .with_dashboard(predefined_dashboard)
        .build()?;

    // =========================================================================
    // Step 4: Build & Start DrasiLib
    // =========================================================================
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("iot-sensor-app")
            .with_source(mock_source)
            .with_query(all_sensors)
            .with_query(hot_sensors)
            .with_query(humid_sensors)
            .with_query(sensor_overview)
            .with_reaction(dashboard_reaction)
            .build()
            .await?,
    );

    core.start().await?;

    println!("\n┌──────────────────────────────────────────────┐");
    println!("│ IoT Sensor Dashboard Started!                 │");
    println!("├──────────────────────────────────────────────┤");
    println!("│ 🌐 Dashboard UI: http://localhost:3000        │");
    println!("├──────────────────────────────────────────────┤");
    println!("│ Mock Source: 10 sensors, readings every 2 s   │");
    println!("│ Queries:                                      │");
    println!("│   • all-sensors     — Full table              │");
    println!("│   • hot-sensors     — Temperature > 27 °C     │");
    println!("│   • humid-sensors   — Humidity > 50 %         │");
    println!("│   • sensor-overview — Aggregation stats       │");
    println!("├──────────────────────────────────────────────┤");
    println!("│ Press Ctrl+C to stop                          │");
    println!("└──────────────────────────────────────────────┘\n");

    tokio::signal::ctrl_c().await?;

    println!("\n>>> Shutting down gracefully...");
    core.stop().await?;
    println!(">>> Shutdown complete.");

    Ok(())
}
