use drasi_query_ast::ast;
use drasi_core::models::{QueryJoin, QueryJoinKey};

/**
apiVersion: query.reactive-graph.io/v1
kind: ContinuousQuery
metadata:
  name: greater_than_a_threshold
spec:
  mode: query
  sources:
    subscriptions:
      - id: Reflex.CRM
        nodes:
          - sourceLabel: Organization
          - sourceLabel: Customer
          - sourceLabel: Call
    joins:
      - id: HAS_CUSTOMER
        keys:
          - label: Organization
            property: id
          - label: Customer
            property: org_id
      - id: MADE_CALL
        keys:
          - label: Customer
            property: id
          - label: Call
            property: cust_id
  query: > … Cypher Query …
*/

pub fn greater_than_a_threshold_metadata() -> Vec<QueryJoin> {
    vec![
        QueryJoin {
            id: "HAS_CUSTOMER".into(),
            keys: vec![
                QueryJoinKey {
                    label: "Organization".into(),
                    property: "id".into(),
                },
                QueryJoinKey {
                    label: "Customer".into(),
                    property: "org_id".into(),
                },
            ],
        },
        QueryJoin {
            id: "MADE_CALL".into(),
            keys: vec![
                QueryJoinKey {
                    label: "Customer".into(),
                    property: "id".into(),
                },
                QueryJoinKey {
                    label: "Call".into(),
                    property: "cust_id".into(),
                },
            ],
        },
    ]
}

pub fn greater_than_a_threshold_query() -> ast::Query {
    drasi_query_cypher::parse(
        "
    MATCH
      (:Organization)-[:HAS_CUSTOMER]->(:Customer)-[:MADE_CALL]->(call:Call {type:'support'})
    WITH
      call,
      datetime( {epochSeconds: call.timestamp } ) AS callDate
    WITH
      callDate.year AS callYear,
      callDate.ordinalDay AS callDayOfYear,
      count(call) AS callCount
    WHERE
      callCount > 10
    RETURN
      callYear, callDayOfYear, callCount",
    )
    .unwrap()
}

pub fn greater_than_a_threshold_by_customer_query() -> ast::Query {
    drasi_query_cypher::parse(
        "
  MATCH
    (:Organization)-[:HAS_CUSTOMER]->(cust:Customer)-[:MADE_CALL]->(call:Call {type:'support'})
  WITH
    cust,
    call,
    datetime( {epochSeconds: call.timestamp } ) AS callDate
  WITH
    elementId(cust) AS customerId,
    cust.name AS customerName,
    callDate.year AS callYear,
    callDate.ordinalDay AS callDayOfYear,
    count(call) AS callCount
  WHERE
    callCount > 5
  RETURN
    customerId, customerName, callYear, callDayOfYear, callCount",
    )
    .unwrap()
}
