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

//! # Orders State Machine Example
//!
//! A self-driving order-lifecycle demo that ties together four Drasi components:
//!
//! ```text
//! Postgres (orders, payments,   ──▶ orders-db source ──▶ 6 stage queries ──┬──▶ state machine source ──▶ dashboard queries ──▶ dashboard
//!           stock_picks, shipments)                                         └──▶ stored-proc reaction ──▶ CALL schedule_advance(...)
//!        ▲                                                                                    │
//!        └───────────────────── pacing driver: SELECT advance_due_orders() ◀──────────────────┘
//! ```
//!
//! Each order flows NEW → CONFIRMED → PAID → PICKED → SHIPPED → DELIVERED. The
//! stages mirror the classic Order / Payment / StockPick / Shipment entities:
//!
//! | Stage     | Relational fact                          | Stage query returns |
//! |-----------|------------------------------------------|---------------------|
//! | NEW       | `orders` row with `is_draft = 1`         | `o.id`              |
//! | CONFIRMED | `orders` row with `is_draft = 0`         | `o.id`              |
//! | PAID      | settled `payments` row                   | `p.order_id`        |
//! | PICKED    | `stock_picks` row                        | `s.order_id`        |
//! | SHIPPED   | `shipments` row                          | `h.order_id`        |
//! | DELIVERED | `shipments` row with `delivered = 1`     | `h.order_id`        |
//!
//! Each stage query is a simple single-entity query returning the owning order id;
//! the **state machine source** subscribes to those queries and correlates them
//! into a per-order lifecycle by that key, exposing each order's live state as its
//! own source that the dashboard queries read. The stored-procedure reaction
//! schedules the next stage whenever an order enters a stage, and a pacing driver
//! applies due advancements on a timer so the dashboard shows a visible
//! progression.
//!
//! ## Running
//!
//! ```bash
//! docker compose up -d        # start Postgres with logical replication
//! cargo run                   # start Drasi + the dashboard on :3002
//! ```
//!
//! Then open <http://localhost:3002> (use <http://127.0.0.1:3002> if your browser
//! resolves `localhost` to IPv6).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_dashboard::{
    DashboardConfig, DashboardReaction, DashboardWidget, GridOptions, WidgetGrid,
};
use drasi_reaction_storedproc_postgres::{PostgresStoredProcReaction, QueryConfig, TemplateSpec};
use drasi_source_postgres::{PostgresReplicationSource, TableKeyConfig};
use drasi_source_state_machine::{StateMachineSourceBuilder, StateMachineSourceConfig};
use drasi_state_store_redb::RedbStateStoreProvider;

// Connect over IPv4 explicitly. On macOS `localhost` resolves to `::1` (IPv6)
// first, but Docker publishes the container port on IPv4 (`0.0.0.0`), so
// connecting to `localhost` could hit `::1` where nothing is listening.
// `127.0.0.1` forces the IPv4 address Docker exposes.
const PG_HOST: &str = "127.0.0.1"; // DevSkim: ignore DS137138
                                   // Host port published by docker-compose (mapped to the container's 5432). Uses a
                                   // distinctive port so it doesn't collide with a local Postgres or another
                                   // container already bound to 5432 on the host.
const PG_PORT: u16 = 5442;
const PG_DB: &str = "drasi";
const PG_USER: &str = "postgres";
const PG_PASSWORD: &str = "postgres";

const SOURCE_ID: &str = "orders-db";
const STATE_SOURCE_ID: &str = "order-state-source";

/// Tables the source ingests (each becomes a node label = the table name).
const TABLES: &[&str] = &["orders", "payments", "stock_picks", "shipments"];

/// The state machine, written exactly as it would appear in YAML configuration.
///
/// Each state lists the query whose results cause an order to enter it, the
/// allowed `previous` states, the Handlebars `key` that extracts the order id
/// from the result row, and the result `ops` that trigger the transition
/// (insert-driven stages use `added`; the is_draft / delivered flag flips also
/// use `updated`).
///
/// Note there is no `sourceId`: the state machine is a single **source**, and its
/// own id (`STATE_SOURCE_ID`, set on the builder below) is what the dashboard
/// queries subscribe to.
const ORDER_STATE_MACHINE: &str = r#"
entityLabel: OrderState
keyField: orderId
states:
  - id: NEW
    enter:
      - query: draft-orders
        previous: []
        key: "{{orderId}}"
        ops: [added]
  - id: CONFIRMED
    enter:
      - query: confirmed-orders
        previous: [NEW]
        key: "{{orderId}}"
        ops: [added, updated]
  - id: PAID
    enter:
      - query: paid-orders
        previous: [CONFIRMED]
        key: "{{orderId}}"
        ops: [added]
  - id: PICKED
    enter:
      - query: picked-orders
        previous: [PAID]
        key: "{{orderId}}"
        ops: [added]
  - id: SHIPPED
    enter:
      - query: shipped-orders
        previous: [PICKED]
        key: "{{orderId}}"
        ops: [added]
  - id: DELIVERED
    enter:
      - query: delivered-orders
        previous: [SHIPPED]
        key: "{{orderId}}"
        ops: [added, updated]
"#;

/// The six stage queries. Each is a single-entity query returning the owning
/// order id as `orderId`; `draft-orders` also carries `customer`/`amount`, which
/// the state machine retains across stages.
fn stage_queries() -> Vec<drasi_lib::config::QueryConfig> {
    let q = |id: &str, cypher: &str| {
        Query::cypher(id)
            .query(cypher)
            .from_source(SOURCE_ID)
            .auto_start(true)
            .build()
    };
    vec![
        q(
            "draft-orders",
            "MATCH (o:orders) WHERE o.is_draft = 1 \
             RETURN o.id AS orderId, o.customer AS customer, o.amount AS amount",
        ),
        q(
            "confirmed-orders",
            "MATCH (o:orders) WHERE o.is_draft = 0 RETURN o.id AS orderId",
        ),
        q(
            "paid-orders",
            "MATCH (p:payments) WHERE p.status = 'settled' RETURN p.order_id AS orderId",
        ),
        q(
            "picked-orders",
            "MATCH (s:stock_picks) RETURN s.order_id AS orderId",
        ),
        q(
            "shipped-orders",
            "MATCH (h:shipments) RETURN h.order_id AS orderId",
        ),
        q(
            "delivered-orders",
            "MATCH (h:shipments) WHERE h.delivered = 1 RETURN h.order_id AS orderId",
        ),
    ]
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("╔══════════════════════════════════════════════╗");
    println!("║   Drasi Orders State Machine Example          ║");
    println!("╚══════════════════════════════════════════════╝\n");

    let tables: Vec<String> = TABLES.iter().map(|t| t.to_string()).collect();
    let table_keys: Vec<TableKeyConfig> = TABLES
        .iter()
        .map(|t| TableKeyConfig {
            table: t.to_string(),
            key_columns: vec!["id".to_string()],
        })
        .collect();

    // -------------------------------------------------------------------------
    // 1. Postgres source (CDC) + Postgres bootstrap provider
    // -------------------------------------------------------------------------
    let mut bootstrap_builder = drasi_bootstrap_postgres::PostgresBootstrapProvider::builder()
        .with_host(PG_HOST)
        .with_port(PG_PORT)
        .with_database(PG_DB)
        .with_user(PG_USER)
        .with_password(PG_PASSWORD)
        .with_tables(tables.clone())
        .with_publication_name("drasi_pub");
    for t in TABLES {
        bootstrap_builder = bootstrap_builder.with_table_key(*t, vec!["id".to_string()]);
    }
    let bootstrap = bootstrap_builder.build();

    let source = PostgresReplicationSource::builder(SOURCE_ID)
        .with_host(PG_HOST)
        .with_port(PG_PORT)
        .with_database(PG_DB)
        .with_user(PG_USER)
        .with_password(PG_PASSWORD)
        .with_tables(tables.clone())
        .with_table_keys(table_keys)
        .with_publication_name("drasi_pub")
        .with_bootstrap_provider(bootstrap)
        .build()?;

    // -------------------------------------------------------------------------
    // 2. The six stage queries
    // -------------------------------------------------------------------------
    let stage_queries = stage_queries();

    // -------------------------------------------------------------------------
    // 3. State machine source (order-state-source), configured declaratively
    //    from the YAML above. It subscribes to the six stage queries and exposes
    //    each order's live state as a source under STATE_SOURCE_ID.
    // -------------------------------------------------------------------------
    let order_state_machine: StateMachineSourceConfig =
        serde_yaml::from_str(ORDER_STATE_MACHINE).expect("valid state machine config");
    let sm_source = StateMachineSourceBuilder::new(STATE_SOURCE_ID)
        .with_config(order_state_machine)
        .build()?;

    // -------------------------------------------------------------------------
    // 4. Stored-procedure reaction: drives orders forward. When an order enters a
    //    stage, schedule its next advancement.
    // -------------------------------------------------------------------------
    let driver = PostgresStoredProcReaction::builder("order-advancer")
        .with_connection(PG_HOST, PG_PORT, PG_DB, PG_USER, PG_PASSWORD)
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new("CALL schedule_advance(@after.orderId)")),
            updated: Some(TemplateSpec::new("CALL schedule_advance(@after.orderId)")),
            deleted: None,
        })
        .with_query("draft-orders")
        .with_query("confirmed-orders")
        .with_query("paid-orders")
        .with_query("picked-orders")
        .with_query("shipped-orders")
        .with_query("delivered-orders")
        .build()
        .await?;

    // -------------------------------------------------------------------------
    // 5. Dashboard queries over the state source + dashboard reaction
    // -------------------------------------------------------------------------
    let all_states_query = Query::cypher("order-states-all")
        .query(
            "MATCH (o:OrderState) \
             RETURN o.orderId AS orderId, o.customer AS customer, \
                    o.state AS state, o.previousState AS previousState",
        )
        .from_source(STATE_SOURCE_ID)
        .auto_start(true)
        .build();

    let delivered_query = Query::cypher("delivered-states")
        .query("MATCH (o:OrderState) WHERE o.state = 'DELIVERED' RETURN o.orderId AS orderId")
        .from_source(STATE_SOURCE_ID)
        .auto_start(true)
        .build();

    let dashboard_reaction = DashboardReaction::builder("orders-dashboard")
        .with_query("order-states-all")
        .with_query("delivered-states")
        // Bind all IPv4 interfaces. On WSL2 this is reachable from Windows both
        // via the WSL2 localhost relay (http://localhost:3002) and directly via
        // the WSL2 IP (http://<wsl-ip>:3002).
        .with_host("0.0.0.0")
        .with_port(3002)
        .with_dashboard(orders_dashboard())
        .build()?;

    // -------------------------------------------------------------------------
    // 6. Durable state store (required by the state machine source)
    // -------------------------------------------------------------------------
    std::fs::create_dir_all("./data")?;
    let state_store = Arc::new(RedbStateStoreProvider::new("./data/state.redb")?);

    // -------------------------------------------------------------------------
    // 7. Assemble and start DrasiLib
    // -------------------------------------------------------------------------
    let mut builder = DrasiLib::builder()
        .with_id("orders-state-machine")
        .with_state_store_provider(state_store)
        .with_source(source)
        .with_source(sm_source)
        .with_query(all_states_query)
        .with_query(delivered_query)
        .with_reaction(driver)
        .with_reaction(dashboard_reaction);
    for q in stage_queries {
        builder = builder.with_query(q);
    }
    let core = Arc::new(builder.build().await?);
    core.start().await?;

    // -------------------------------------------------------------------------
    // 8. Background tasks: pacing driver and order seeder
    // -------------------------------------------------------------------------
    let pacing = tokio::spawn(pacing_driver());
    let seeder = tokio::spawn(order_seeder());

    println!("\n┌──────────────────────────────────────────────┐");
    println!("│ Orders State Machine started!                 │");
    println!("├──────────────────────────────────────────────┤");
    println!("│ 🌐 Dashboard:  http://localhost:3002          │");
    println!("│ 🗄  Postgres:   127.0.0.1:5442 (db: drasi)     │");
    println!("│ Orders advance one stage roughly every 1 s.   │");
    println!("│ A new order is seeded every 8 s.              │");
    println!("├──────────────────────────────────────────────┤");
    println!("│ Press Ctrl+C to stop                          │");
    println!("└──────────────────────────────────────────────┘\n");

    tokio::signal::ctrl_c().await?;
    println!("\n>>> Shutting down...");
    pacing.abort();
    seeder.abort();
    core.stop().await?;
    println!(">>> Shutdown complete.");
    Ok(())
}

/// Connect to Postgres for the helper tasks below.
async fn connect() -> Result<tokio_postgres::Client> {
    let conn_str = format!(
        "host={PG_HOST} port={PG_PORT} dbname={PG_DB} user={PG_USER} password={PG_PASSWORD}"
    );
    let (client, connection) = tokio_postgres::connect(&conn_str, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            log::warn!("postgres helper connection error: {e}");
        }
    });
    Ok(client)
}

/// Apply due stage advancements twice a second so orders progress visibly.
async fn pacing_driver() {
    let client = match connect().await {
        Ok(c) => c,
        Err(e) => {
            log::error!("pacing driver could not connect to Postgres: {e}");
            return;
        }
    };
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    loop {
        interval.tick().await;
        if let Err(e) = client.execute("SELECT advance_due_orders()", &[]).await {
            log::warn!("advance_due_orders failed: {e}");
        }
    }
}

/// Seed a new draft order every few seconds to keep the pipeline flowing.
async fn order_seeder() {
    let client = match connect().await {
        Ok(c) => c,
        Err(e) => {
            log::error!("seeder could not connect to Postgres: {e}");
            return;
        }
    };
    let customers = [
        "Acme Corp",
        "Globex",
        "Initech",
        "Umbrella",
        "Hooli",
        "Stark Ind",
    ];
    let mut n = 1u64;
    let mut interval = tokio::time::interval(Duration::from_secs(8));
    loop {
        interval.tick().await;
        let id = format!("order-{n}");
        let customer = customers[(n as usize) % customers.len()];
        let amount = ((n * 37) % 500) as f64 + 25.0;
        match client
            .execute("CALL new_order($1, $2, $3)", &[&id, &customer, &amount])
            .await
        {
            Ok(_) => log::info!("seeded new draft {id} for {customer}"),
            Err(e) => log::warn!("seeding order {id} failed: {e}"),
        }
        n += 1;
    }
}

/// Build the predefined dashboard: a table of order states plus KPIs.
fn orders_dashboard() -> DashboardConfig {
    DashboardConfig::with_id(
        "orders-monitor",
        "Order Lifecycle".to_string(),
        GridOptions::default(),
        vec![
            DashboardWidget {
                id: "w-table-states".to_string(),
                widget_type: "table".to_string(),
                title: "Order States".to_string(),
                grid: WidgetGrid {
                    x: 0,
                    y: 0,
                    w: 8,
                    h: 6,
                },
                config: serde_json::json!({
                    "queryId": "order-states-all",
                    "columns": ["orderId", "customer", "state", "previousState"]
                }),
            },
            DashboardWidget {
                id: "w-kpi-total".to_string(),
                widget_type: "kpi".to_string(),
                title: "Total Orders".to_string(),
                grid: WidgetGrid {
                    x: 8,
                    y: 0,
                    w: 4,
                    h: 3,
                },
                config: serde_json::json!({
                    "queryId": "order-states-all",
                    "valueField": "orderId",
                    "aggregation": "count",
                    "label": "Tracked Orders"
                }),
            },
            DashboardWidget {
                id: "w-kpi-delivered".to_string(),
                widget_type: "kpi".to_string(),
                title: "Delivered".to_string(),
                grid: WidgetGrid {
                    x: 8,
                    y: 3,
                    w: 4,
                    h: 3,
                },
                config: serde_json::json!({
                    "queryId": "delivered-states",
                    "valueField": "orderId",
                    "aggregation": "count",
                    "label": "Delivered Orders"
                }),
            },
        ],
    )
}
