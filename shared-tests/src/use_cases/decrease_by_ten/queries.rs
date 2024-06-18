
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

pub fn decrease_by_ten_percent_query() -> &'static str  {
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
