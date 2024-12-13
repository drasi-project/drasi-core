// Copyright 2024 The Drasi Authors.
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

use drasi_core::models::{QueryJoin, QueryJoinKey};

/**
apiVersion: query.reactive-graph.io/v1
kind: ContinuousQuery
metadata:
  name: decrease_by_ten
spec:
  mode: query
  sources:
    subscriptions:
      - id: Reflex.Sales
        nodes:
          - sourceLabel: Product
          - sourceLabel: DailyRevenue
          - sourceLabel: Employee
    joins:
      - id: HAS_DAILY_REVENUE
        keys:
          - label: Product
            property: id
          - label: DailyRevenue
            property: prod_id
      - id: HAS_PRODUCT_MANAGER
        keys:
          - label: Product
            property: pm_id
          - label: Employee
            property: id
  query: > … Cypher Query …
*/
pub fn decrease_by_ten_metadata() -> Vec<QueryJoin> {
    vec![
        QueryJoin {
            id: "HAS_DAILY_REVENUE".into(),
            keys: vec![
                QueryJoinKey {
                    label: "Product".into(),
                    property: "id".into(),
                },
                QueryJoinKey {
                    label: "DailyRevenue".into(),
                    property: "prod_id".into(),
                },
            ],
        },
        QueryJoin {
            id: "HAS_PRODUCT_MANAGER".into(),
            keys: vec![
                QueryJoinKey {
                    label: "Product".into(),
                    property: "prod_mgr_id".into(),
                },
                QueryJoinKey {
                    label: "Employee".into(),
                    property: "id".into(),
                },
            ],
        },
    ]
}

pub fn decrease_by_ten_query() -> &'static str {
    "
    MATCH
      (product:Product)-[:HAS_DAILY_REVENUE]->(dailyRevenue:DailyRevenue),
      (product:Product)-[:HAS_PRODUCT_MANAGER]->(productManager:Employee)
    WITH
      product,    
      productManager,
      dailyRevenue,
      drasi.getVersionByTimestamp(dailyRevenue, dailyRevenue.timestamp - 1 ) AS previousDailyRevenue
    WHERE
      dailyRevenue.amount < previousDailyRevenue.amount - 10000
    RETURN
      product.id AS productId, 
      productManager.id AS productManagerId,
      previousDailyRevenue.amount AS previousDailyRevenue,
      dailyRevenue.amount AS dailyRevenue"
}

pub fn decrease_by_ten_percent_query() -> &'static str {
    "
    MATCH
      (product:Product)-[:HAS_DAILY_REVENUE]->(dailyRevenue:DailyRevenue),
      (product:Product)-[:HAS_PRODUCT_MANAGER]->(productManager:Employee)
    WITH
      product,    
      productManager,
      dailyRevenue,
      drasi.getVersionByTimestamp(dailyRevenue, dailyRevenue.timestamp - 1 ) AS previousDailyRevenue
    WHERE
      dailyRevenue.amount < previousDailyRevenue.amount * 0.9
    RETURN
      product.id AS productId, 
      productManager.id AS productManagerId,
      previousDailyRevenue.amount AS previousDailyRevenue,
      dailyRevenue.amount AS dailyRevenue"
}
