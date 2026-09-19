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

use std::{collections::HashMap, sync::Arc};

use serde_json::{json, Value};

use super::{aggregate_update_tests::assert_summary_delta, materialized_query::MaterializedQuery};
use crate::{
    evaluation::{context::QueryVariables, variable_value::VariableValue},
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, QueryJoin, QueryJoinKey,
        SourceChange,
    },
};

// Trading's production query and both joins, adapted from #810's library test
// to the released engine API without library hydration or checkpoint changes.
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

fn join(id: &str, keys: &[(&str, &str)]) -> QueryJoin {
    QueryJoin {
        id: id.to_string(),
        keys: keys
            .iter()
            .map(|(label, property)| QueryJoinKey {
                label: label.to_string(),
                property: property.to_string(),
            })
            .collect(),
    }
}

async fn portfolio_query() -> MaterializedQuery {
    MaterializedQuery::with_joins(
        PORTFOLIO_SUMMARY,
        vec![
            join(
                "OWNS_STOCK",
                &[("portfolio", "symbol"), ("stocks", "symbol")],
            ),
            join(
                "HAS_PRICE",
                &[("stocks", "symbol"), ("stock_prices", "symbol")],
            ),
        ],
    )
    .await
}

fn summary(total_value: f64, total_cost: f64, position_count: i64) -> QueryVariables {
    QueryVariables::from([
        ("totalValue".into(), VariableValue::from(json!(total_value))),
        ("totalCost".into(), VariableValue::from(json!(total_cost))),
        (
            "totalProfitLoss".into(),
            VariableValue::from(json!(total_value - total_cost)),
        ),
        (
            "totalProfitLossPercent".into(),
            VariableValue::from(json!((total_value - total_cost) / total_cost * 100.0)),
        ),
        (
            "positionCount".into(),
            VariableValue::from(json!(position_count)),
        ),
    ])
}

#[tokio::test]
async fn aggregate_snapshot_trading_bootstrap_live_diffs_and_fresh_reconstruction() {
    let mut subject = portfolio_query().await;
    for element in portfolio_rows() {
        subject.process(SourceChange::Insert { element }).await;
    }
    assert!(subject.rows.is_empty());

    let first = subject
        .process(SourceChange::Insert {
            element: price("AAPL", 110, 1),
        })
        .await;
    assert_eq!(first.len(), 1);
    let signature = first[0].row_signature();
    assert_summary_delta(&first, signature, None, Some(summary(1100.0, 800.0, 1)));
    assert_eq!(
        subject.rows,
        HashMap::from([(signature, summary(1100.0, 800.0, 1))])
    );

    let second = subject
        .process(SourceChange::Insert {
            element: price("MSFT", 180, 1),
        })
        .await;
    assert_eq!(
        subject.rows,
        HashMap::from([(signature, summary(2000.0, 1800.0, 2))]),
        "Trading bootstrap must materialize exactly one current summary"
    );
    assert_summary_delta(
        &second,
        signature,
        Some(summary(1100.0, 800.0, 1)),
        Some(summary(2000.0, 1800.0, 2)),
    );

    for (time, price_value, before, after) in [(2, 115, 2000.0, 2050.0), (3, 125, 2050.0, 2150.0)] {
        let changes = subject
            .process(SourceChange::Update {
                element: price("AAPL", price_value, time),
            })
            .await;
        assert_summary_delta(
            &changes,
            signature,
            Some(summary(before, 1800.0, 2)),
            Some(summary(after, 1800.0, 2)),
        );
        assert_eq!(
            subject.rows,
            HashMap::from([(signature, summary(after, 1800.0, 2))])
        );
    }

    let mut fresh = portfolio_query().await;
    let mut current = portfolio_rows();
    current.extend([price("AAPL", 125, 3), price("MSFT", 180, 1)]);
    // Preserve the joined fixture's source order; the reduced contribution test
    // separately covers reverse-order reconstruction without synthetic joins.
    for element in current {
        fresh.process(SourceChange::Insert { element }).await;
    }
    assert_eq!(subject.rows, fresh.rows);

    // A contributing-position deletion, not Trading's watchlist-only deletion.
    for (id, time, before, after) in [
        (
            "2",
            4,
            summary(2150.0, 1800.0, 2),
            Some(summary(1250.0, 800.0, 1)),
        ),
        ("1", 5, summary(1250.0, 800.0, 1), None),
    ] {
        let changes = subject
            .process(SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("postgres-stocks", id),
                    labels: Arc::from([Arc::from("portfolio")]),
                    effective_from: time,
                },
            })
            .await;
        assert_summary_delta(&changes, signature, Some(before), after.clone());
        let expected = after
            .map(|row| HashMap::from([(signature, row)]))
            .unwrap_or_default();
        assert_eq!(subject.rows, expected);
    }
    assert!(subject.rows.is_empty());
}

fn room(id: &str, floor: &str, temperature: i64, time: u64) -> Element {
    node(
        "building",
        "Room",
        id,
        time,
        json!({"floor_id": floor, "temperature": temperature, "humidity": 40, "co2": 10}),
    )
}

fn comfort_row(floor: &str, comfort: f64) -> QueryVariables {
    QueryVariables::from([
        ("FloorId".into(), VariableValue::from(json!(floor))),
        ("ComfortLevel".into(), VariableValue::from(json!(comfort))),
    ])
}

#[tokio::test]
async fn aggregate_snapshot_original_floor_comfort_preserves_independent_groups() {
    let mut subject = MaterializedQuery::with_joins(
        "MATCH (r:Room)-[:PART_OF_FLOOR]->(f:Floor)
         WITH f, floor(50 + (r.temperature - 72) + (r.humidity - 42)
              + CASE WHEN r.co2 > 500 THEN (r.co2 - 500) / 25 ELSE 0 END) AS RoomComfortLevel
         WITH f, avg(RoomComfortLevel) AS ComfortLevel
         RETURN f.id AS FloorId, ComfortLevel",
        vec![join(
            "PART_OF_FLOOR",
            &[("Room", "floor_id"), ("Floor", "id")],
        )],
    )
    .await;

    for floor in ["floor-a", "floor-b"] {
        subject
            .process(SourceChange::Insert {
                element: node("building", "Floor", floor, 1, json!({"id": floor})),
            })
            .await;
        for i in 1..=3 {
            subject
                .process(SourceChange::Insert {
                    element: room(&format!("{floor}-room-{i}"), floor, 70, 1),
                })
                .await;
        }
    }

    assert_eq!(subject.rows.len(), 2);
    let signatures: HashMap<&str, u64> = ["floor-a", "floor-b"]
        .into_iter()
        .map(|floor| {
            let signature = subject
                .rows
                .iter()
                .find_map(|(signature, row)| {
                    (row == &comfort_row(floor, 46.0)).then_some(*signature)
                })
                .expect("each floor must have its own current row");
            (floor, signature)
        })
        .collect();
    assert_ne!(signatures["floor-a"], signatures["floor-b"]);
    let mut expected = HashMap::from([
        (signatures["floor-a"], comfort_row("floor-a", 46.0)),
        (signatures["floor-b"], comfort_row("floor-b", 46.0)),
    ]);
    assert_eq!(subject.rows, expected);

    for (time, floor, room_id, temperature, comfort) in [
        (2, "floor-a", "floor-a-room-1", 82, 50.0),
        (3, "floor-a", "floor-a-room-2", 82, 54.0),
        (4, "floor-b", "floor-b-room-1", 82, 50.0),
        (5, "floor-a", "floor-a-room-1", 70, 50.0),
    ] {
        let signature = signatures[floor];
        let before = expected[&signature].clone();
        let after = comfort_row(floor, comfort);
        let changes = subject
            .process(SourceChange::Update {
                element: room(room_id, floor, temperature, time),
            })
            .await;
        assert_summary_delta(&changes, signature, Some(before), Some(after.clone()));
        expected.insert(signature, after);
        assert_eq!(subject.rows, expected);
    }
}
