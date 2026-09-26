// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Regressions for #680 through bootstrap, live diffs, and DrasiLib snapshots.

mod mock_source;

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::{
    bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult},
    channels::{BootstrapEvent, BootstrapEventSender, QuerySubscriptionResponse, ResultDiff},
    config::{QueryConfig, QueryJoinConfig, QueryJoinKeyConfig, SourceSubscriptionSettings},
    queries::Query as RunningQuery,
    DrasiLib, Query, Source,
};
use mock_source::MockSource;
use serde_json::{json, Value};
use tokio::{sync::RwLock, time::timeout};
use tokio_stream::StreamExt;

const TIMEOUT: Duration = Duration::from_secs(10);
const SUMMARY_ID: &str = "portfolio-summary-query";

// The production Trading query and joins from drasi-server#201's recorded
// a2b6480-core-0.5.8 fixture. Only source transport is replaced by test channels.
const PORTFOLIO_SUMMARY: &str = "
    MATCH (p:portfolio)-[:OWNS_STOCK]->(s:stocks)-[:HAS_PRICE]->(sp:stock_prices)
    WITH sum(sp.price * p.quantity) AS totalValue,
         sum(toFloat(p.purchase_price) * p.quantity) AS totalCost,
         count(p) AS positionCount
    RETURN totalValue,
           totalCost,
           (totalValue - totalCost) AS totalProfitLoss,
           CASE WHEN totalCost > 0
                THEN ((totalValue - totalCost) / totalCost * 100)
                ELSE 0
           END AS totalProfitLossPercent,
           positionCount
";

struct BootstrapRows(Arc<RwLock<Vec<Element>>>);

#[async_trait]
impl BootstrapProvider for BootstrapRows {
    async fn bootstrap(
        &self,
        _request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        let elements = self.0.read().await.clone();
        let event_count = elements.len();
        for element in elements {
            event_tx
                .send(BootstrapEvent {
                    source_id: context.source_id.clone(),
                    change: SourceChange::Insert { element },
                    timestamp: chrono::Utc::now(),
                    sequence: context.next_sequence(),
                })
                .await?;
        }
        Ok(BootstrapResult {
            event_count,
            source_position: None,
        })
    }
}

fn node(source: &str, label: &str, id: &str, time: u64, properties: Value) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source, id),
            labels: Arc::from([Arc::from(label)]),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(properties),
    }
}

fn price(symbol: &str, price: i64, time: u64) -> Element {
    node(
        "price-feed",
        "stock_prices",
        symbol,
        time,
        json!({"symbol": symbol, "price": price}),
    )
}

fn portfolio_rows() -> Vec<Element> {
    vec![
        node(
            "postgres-stocks",
            "stocks",
            "AAPL",
            1,
            json!({"symbol": "AAPL"}),
        ),
        node(
            "postgres-stocks",
            "stocks",
            "MSFT",
            1,
            json!({"symbol": "MSFT"}),
        ),
        node(
            "postgres-stocks",
            "portfolio",
            "1",
            1,
            json!({"id": 1, "symbol": "AAPL", "quantity": 10, "purchase_price": "80"}),
        ),
        node(
            "postgres-stocks",
            "portfolio",
            "2",
            1,
            json!({"id": 2, "symbol": "MSFT", "quantity": 5, "purchase_price": "200"}),
        ),
    ]
}

fn join(id: &str, keys: &[(&str, &str)]) -> QueryJoinConfig {
    QueryJoinConfig {
        id: id.to_string(),
        keys: keys
            .iter()
            .map(|(label, property)| QueryJoinKeyConfig {
                label: label.to_string(),
                property: property.to_string(),
            })
            .collect(),
    }
}

fn portfolio_config() -> QueryConfig {
    Query::cypher(SUMMARY_ID)
        .query(PORTFOLIO_SUMMARY)
        .from_source("postgres-stocks")
        .from_source("price-feed")
        .with_joins(vec![
            join(
                "OWNS_STOCK",
                &[("portfolio", "symbol"), ("stocks", "symbol")],
            ),
            join(
                "HAS_PRICE",
                &[("stocks", "symbol"), ("stock_prices", "symbol")],
            ),
        ])
        .build()
}

fn summary(total_value: f64, total_cost: f64, position_count: i64) -> Value {
    json!({
        "totalValue": total_value,
        "totalCost": total_cost,
        "totalProfitLoss": total_value - total_cost,
        "totalProfitLossPercent": (total_value - total_cost) / total_cost * 100.0,
        "positionCount": position_count,
    })
}

async fn keyed_snapshot(query: &dyn RunningQuery) -> Result<HashMap<u64, Value>> {
    let snapshot = timeout(TIMEOUT, query.fetch_snapshot()).await??;
    Ok(snapshot.stream_keyed().collect().await)
}

async fn assert_summary_snapshot(
    core: &DrasiLib,
    query: &dyn RunningQuery,
    expected: Value,
) -> Result<u64> {
    let rows = keyed_snapshot(query).await?;
    assert_eq!(
        core.get_query_results(SUMMARY_ID).await?,
        vec![expected.clone()],
        "get_query_results must return exactly the current aggregate (keyed snapshot: {rows:?})"
    );
    assert_eq!(
        rows.len(),
        1,
        "snapshot must not retain historical aggregates: {rows:?}"
    );
    let (signature, row) = rows
        .into_iter()
        .next()
        .expect("the global aggregate snapshot must contain one row");
    assert_eq!(row, expected);
    Ok(signature)
}

async fn assert_live_update(
    subscription: &mut QuerySubscriptionResponse,
    signature: u64,
    before: Value,
    after: Value,
) -> Result<()> {
    let result = timeout(TIMEOUT, subscription.receiver.recv()).await??;
    assert_eq!(
        result.results,
        vec![ResultDiff::Update {
            data: after.clone(),
            before,
            after,
            grouping_keys: None,
            row_signature: signature,
        }]
    );
    Ok(())
}

#[tokio::test]
async fn trading_bootstrap_reuses_the_same_solver_in_both_source_orders() -> Result<()> {
    use drasi_core::{
        evaluation::{context::QueryPartEvaluationContext, functions::FunctionRegistry},
        query::QueryBuilder,
    };
    use drasi_functions_cypher::CypherFunctionSet;
    use drasi_query_cypher::CypherParser;
    for prices_first in [false, true] {
        let functions = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let query = QueryBuilder::new(
            PORTFOLIO_SUMMARY,
            Arc::new(CypherParser::new(functions.clone())),
        )
        .with_function_registry(functions)
        .with_joins(
            portfolio_config()
                .joins
                .unwrap()
                .into_iter()
                .map(Into::into)
                .collect(),
        )
        .try_build()
        .await?;
        let prices = vec![price("AAPL", 110, 1), price("MSFT", 180, 1)];
        let changes = if prices_first {
            prices
                .into_iter()
                .chain(portfolio_rows())
                .collect::<Vec<_>>()
        } else {
            portfolio_rows()
                .into_iter()
                .chain(prices)
                .collect::<Vec<_>>()
        };
        let mut last = None;
        for element in changes {
            for diff in query
                .process_source_change(SourceChange::Insert { element })
                .await?
            {
                match diff {
                    QueryPartEvaluationContext::Adding { after, .. }
                    | QueryPartEvaluationContext::Updating { after, .. }
                    | QueryPartEvaluationContext::Aggregation { after, .. } => {
                        last = Some(serde_json::to_value(after)?);
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(
            last,
            Some(summary(2000.0, 1800.0, 2)),
            "prices_first={prices_first}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn aggregate_snapshot_trading_bootstrap_updates_deletes_and_rebootstrap() -> Result<()> {
    let postgres_rows = Arc::new(RwLock::new(portfolio_rows()));
    let price_rows = Arc::new(RwLock::new(vec![
        price("AAPL", 110, 1),
        price("MSFT", 180, 1),
    ]));
    let (postgres, database) = MockSource::new("postgres-stocks")?;
    postgres
        .set_bootstrap_provider(Box::new(BootstrapRows(postgres_rows.clone())))
        .await;
    let (prices, price_feed) = MockSource::new("price-feed")?;
    prices
        .set_bootstrap_provider(Box::new(BootstrapRows(price_rows)))
        .await;
    let core = DrasiLib::builder()
        .with_id("aggregate-snapshot-trading")
        .with_source(postgres)
        .with_source(prices)
        .with_query(portfolio_config())
        .build()
        .await?;
    core.start().await?;
    let query = core
        .query_manager()
        .get_query_instance(SUMMARY_ID)
        .await
        .map_err(anyhow::Error::msg)?;
    let signature =
        assert_summary_snapshot(&core, query.as_ref(), summary(2000.0, 1800.0, 2)).await?;
    assert_eq!(query.fetch_snapshot().await?.as_of_sequence, 0);
    let mut subscription = query.subscribe("snapshot-regression".to_string()).await?;

    for (time, price_value, before, after) in [(2, 115, 2000.0, 2050.0), (3, 125, 2050.0, 2150.0)] {
        price_feed
            .send(SourceChange::Update {
                element: price("AAPL", price_value, time),
            })
            .await?;
        assert_live_update(
            &mut subscription,
            signature,
            summary(before, 1800.0, 2),
            summary(after, 1800.0, 2),
        )
        .await?;
        assert_eq!(
            assert_summary_snapshot(&core, query.as_ref(), summary(after, 1800.0, 2)).await?,
            signature
        );
    }

    // This is an additional contributing-position deletion, not the recorded
    // Trading scenario's MSFT watchlist deletion (which leaves its position).
    database
        .send(SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("postgres-stocks", "2"),
                labels: Arc::from([Arc::from("portfolio")]),
                effective_from: 4,
            },
        })
        .await?;
    assert_live_update(
        &mut subscription,
        signature,
        summary(2150.0, 1800.0, 2),
        summary(1250.0, 800.0, 1),
    )
    .await?;
    assert_eq!(
        assert_summary_snapshot(&core, query.as_ref(), summary(1250.0, 800.0, 1)).await?,
        signature
    );

    database
        .send(SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("postgres-stocks", "1"),
                labels: Arc::from([Arc::from("portfolio")]),
                effective_from: 5,
            },
        })
        .await?;
    let deletion = timeout(TIMEOUT, subscription.receiver.recv()).await??;
    assert_eq!(
        deletion.results,
        vec![ResultDiff::Delete {
            data: summary(1250.0, 800.0, 1),
            row_signature: signature,
        }]
    );
    assert!(keyed_snapshot(query.as_ref()).await?.is_empty());
    assert!(core.get_query_results(SUMMARY_ID).await?.is_empty());
    drop(subscription);
    drop(query);

    // Explicitly recreate the in-memory query, first from an empty portfolio,
    // then from a complete snapshot. Stop/start alone is not a reset contract.
    core.remove_query(SUMMARY_ID).await?;
    postgres_rows.write().await.truncate(2);
    core.add_query(portfolio_config()).await?;
    let query = core
        .query_manager()
        .get_query_instance(SUMMARY_ID)
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(keyed_snapshot(query.as_ref()).await?.is_empty());
    assert!(core.get_query_results(SUMMARY_ID).await?.is_empty());
    drop(query);

    core.remove_query(SUMMARY_ID).await?;
    *postgres_rows.write().await = portfolio_rows();
    core.add_query(portfolio_config()).await?;
    let query = core
        .query_manager()
        .get_query_instance(SUMMARY_ID)
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        assert_summary_snapshot(&core, query.as_ref(), summary(2000.0, 1800.0, 2)).await?,
        signature
    );
    core.shutdown().await?;
    Ok(())
}

fn room(id: &str, floor: &str, temperature: i64, time: u64) -> Element {
    node(
        "building",
        "Room",
        id,
        time,
        json!({
            "floor_id": floor, "temperature": temperature, "humidity": 40, "co2": 10,
        }),
    )
}

#[tokio::test]
async fn aggregate_snapshot_floor_comfort_keeps_independent_equal_valued_groups() -> Result<()> {
    // The query from #680, reduced from three floors to two independent groups.
    let query_text = "
        MATCH (r:Room)-[:PART_OF_FLOOR]->(f:Floor)
        WITH f, floor(50 + (r.temperature - 72) + (r.humidity - 42)
             + CASE WHEN r.co2 > 500 THEN (r.co2 - 500) / 25 ELSE 0 END) AS RoomComfortLevel
        WITH f, avg(RoomComfortLevel) AS ComfortLevel
        RETURN f.id AS FloorId, ComfortLevel
    ";
    let mut rows = Vec::new();
    for floor in ["floor-a", "floor-b"] {
        rows.push(node("building", "Floor", floor, 1, json!({"id": floor})));
        for i in 1..=3 {
            rows.push(room(&format!("{floor}-room-{i}"), floor, 70, 1));
        }
    }
    let (source, handle) = MockSource::new("building")?;
    source
        .set_bootstrap_provider(Box::new(BootstrapRows(Arc::new(RwLock::new(rows)))))
        .await;
    let core = DrasiLib::builder()
        .with_id("aggregate-snapshot-floor-comfort")
        .with_source(source)
        .with_query(
            Query::cypher("comfort")
                .query(query_text)
                .from_source("building")
                .with_joins(vec![join(
                    "PART_OF_FLOOR",
                    &[("Room", "floor_id"), ("Floor", "id")],
                )])
                .build(),
        )
        .build()
        .await?;
    core.start().await?;
    let query = core
        .query_manager()
        .get_query_instance("comfort")
        .await
        .map_err(anyhow::Error::msg)?;
    let initial = keyed_snapshot(query.as_ref()).await?;
    assert_eq!(initial.len(), 2);
    let signatures: HashMap<String, u64> = initial
        .iter()
        .map(|(signature, row)| (row["FloorId"].as_str().unwrap().to_string(), *signature))
        .collect();
    assert_ne!(signatures["floor-a"], signatures["floor-b"]);
    let mut expected = HashMap::from([
        (
            signatures["floor-a"],
            json!({"FloorId": "floor-a", "ComfortLevel": 46.0}),
        ),
        (
            signatures["floor-b"],
            json!({"FloorId": "floor-b", "ComfortLevel": 46.0}),
        ),
    ]);
    assert_eq!(initial, expected);
    let mut subscription = query.subscribe("comfort-regression".to_string()).await?;
    for (time, floor, room_id, temperature, comfort) in [
        (2, "floor-a", "floor-a-room-1", 82, 50.0),
        (3, "floor-a", "floor-a-room-2", 82, 54.0),
        (4, "floor-b", "floor-b-room-1", 82, 50.0),
        (5, "floor-a", "floor-a-room-1", 70, 50.0),
    ] {
        let signature = signatures[floor];
        let before = expected[&signature].clone();
        let after = json!({"FloorId": floor, "ComfortLevel": comfort});
        handle
            .send(SourceChange::Update {
                element: room(room_id, floor, temperature, time),
            })
            .await?;
        assert_live_update(&mut subscription, signature, before, after.clone()).await?;
        expected.insert(signature, after);
        assert_eq!(keyed_snapshot(query.as_ref()).await?, expected);
        let mut actual = core.get_query_results("comfort").await?;
        actual.sort_by_key(|row| row["FloorId"].as_str().unwrap().to_string());
        assert_eq!(
            actual,
            vec![
                expected[&signatures["floor-a"]].clone(),
                expected[&signatures["floor-b"]].clone()
            ]
        );
    }
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn grouping_numeric_public_snapshot_updates_one_row() -> Result<()> {
    let (source, handle) = MockSource::new("numeric")?;
    let core = DrasiLib::builder()
        .with_id("numeric-identity")
        .with_source(source)
        .with_query(Query::cypher("numeric-summary")
            .query("MATCH (p:Position) WITH p.group AS groupKey, sum(p.value) AS totalValue RETURN groupKey, totalValue")
            .from_source("numeric")
            .enable_bootstrap(false)
            .build())
        .build().await?;
    core.start().await?;
    let query = core
        .query_manager()
        .get_query_instance("numeric-summary")
        .await
        .map_err(anyhow::Error::msg)?;
    let mut subscription = query.subscribe("numeric-test".into()).await?;
    handle
        .send(SourceChange::Insert {
            element: node(
                "numeric",
                "Position",
                "p",
                1,
                json!({"group": 1, "value": 10}),
            ),
        })
        .await?;
    let first = timeout(TIMEOUT, subscription.receiver.recv()).await??;
    let signature = match first.results.as_slice() {
        [ResultDiff::Add { row_signature, .. }] => *row_signature,
        other => panic!("expected one added group, got {other:?}"),
    };
    handle
        .send(SourceChange::Update {
            element: node(
                "numeric",
                "Position",
                "p",
                2,
                json!({"group": 1.0, "value": 20}),
            ),
        })
        .await?;
    let update = timeout(TIMEOUT, subscription.receiver.recv()).await??;
    assert_eq!(
        core.get_query_results("numeric-summary").await?,
        vec![json!({"groupKey": 1.0, "totalValue": 20.0})],
        "numeric representation changes must not retain old aggregate rows",
    );
    assert!(
        matches!(update.results.as_slice(), [ResultDiff::Update { row_signature, .. }] if *row_signature == signature)
    );
    handle
        .send(SourceChange::Update {
            element: node(
                "numeric",
                "Position",
                "p",
                3,
                json!({"group": 1, "value": 30}),
            ),
        })
        .await?;
    assert_live_update(
        &mut subscription,
        signature,
        json!({"groupKey": 1.0, "totalValue": 20.0}),
        json!({"groupKey": 1, "totalValue": 30.0}),
    )
    .await?;
    assert_eq!(
        core.get_query_results("numeric-summary").await?,
        vec![json!({"groupKey": 1, "totalValue": 30.0})]
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn terminal_aggregate_public_notifications_skip_unchanged_zero() -> Result<()> {
    let (source, handle) = MockSource::new("terminal")?;
    let core = DrasiLib::builder()
        .with_id("terminal-identity")
        .with_source(source)
        .with_query(
            Query::cypher("terminal-summary")
                .query("MATCH (p:Position) RETURN sum(p.value) AS totalValue")
                .from_source("terminal")
                .enable_bootstrap(false)
                .build(),
        )
        .build()
        .await?;
    core.start().await?;
    let query = core
        .query_manager()
        .get_query_instance("terminal-summary")
        .await
        .map_err(anyhow::Error::msg)?;
    let mut subscription = query.subscribe("terminal-test".into()).await?;
    let zero = node("terminal", "Position", "zero", 1, json!({"value": 0}));
    handle
        .send(SourceChange::Insert {
            element: zero.clone(),
        })
        .await?;
    let first = timeout(TIMEOUT, subscription.receiver.recv()).await??;
    assert_eq!(first.sequence, 1);
    assert!(
        matches!(first.results.as_slice(), [ResultDiff::Aggregation { after, .. }]
        if after == &json!({"totalValue": 0.0}))
    );
    assert_eq!(
        core.get_query_results("terminal-summary").await?,
        vec![json!({"totalValue": 0.0})]
    );

    handle
        .send(SourceChange::Delete {
            metadata: zero.get_metadata().clone(),
        })
        .await?;
    // A subsequent real change is a processing barrier: no settling sleep or
    // timeout-as-success is needed to assert absence of the zero deletion event.
    handle
        .send(SourceChange::Insert {
            element: node("terminal", "Position", "next", 3, json!({"value": 5})),
        })
        .await?;
    let next = timeout(TIMEOUT, subscription.receiver.recv()).await??;
    assert!(
        matches!(next.results.as_slice(), [ResultDiff::Aggregation { before: Some(before), after, .. }]
        if before == &json!({"totalValue": 0.0}) && after == &json!({"totalValue": 5.0})),
        "the next notification must be a real value change, not 0 -> 0: {:?}",
        next.results
    );
    assert_eq!(next.sequence, 2);
    assert_eq!(query.fetch_outbox(0).await?.results.len(), 2);
    assert_eq!(
        core.get_query_results("terminal-summary").await?,
        vec![json!({"totalValue": 5.0})]
    );
    handle
        .send(SourceChange::Delete {
            metadata: node("terminal", "Position", "next", 4, json!({"value": 5}))
                .get_metadata()
                .clone(),
        })
        .await?;
    let deleted = timeout(TIMEOUT, subscription.receiver.recv()).await??;
    assert_eq!(deleted.sequence, 3);
    assert!(
        matches!(deleted.results.as_slice(), [ResultDiff::Aggregation {
        before: Some(before), after, ..
    }] if before == &json!({"totalValue": 5.0}) && after == &json!({"totalValue": 0.0}))
    );
    assert_eq!(
        core.get_query_results("terminal-summary").await?,
        vec![json!({"totalValue": 0.0})]
    );
    core.shutdown().await?;
    Ok(())
}
