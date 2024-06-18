
use drasi_core::models::{QueryJoin, QueryJoinKey};

/**
apiVersion: query.reactive-graph.io/v1
kind: ContinuousQuery
metadata:
  name: steps_happen_in_any_order
spec:
  mode: query
  sources:
    subscriptions:
      - id: Reflex.CRM
        nodes:
          - sourceLabel: Customer
          - sourceLabel: Step
          - sourceLabel: CompletedStep
    joins:
      - id: COMPLETES_STEP
        keys:
          - label: Customer
            property: id
          - label: CompletedStep
            property: cust_id
      - id: IS_STEP
        keys:
          - label: Step
            property: id
          - label: CompletedStep
            property: step_id
  query: > … Cypher Query …
*/

pub fn steps_happen_in_any_order_metadata() -> Vec<QueryJoin> {
    vec![
        QueryJoin {
            id: "COMPLETES_STEP".into(),
            keys: vec![
                QueryJoinKey {
                    label: "Customer".into(),
                    property: "id".into(),
                },
                QueryJoinKey {
                    label: "CompletedStep".into(),
                    property: "cust_id".into(),
                },
            ],
        },
        QueryJoin {
            id: "IS_STEP".into(),
            keys: vec![
                QueryJoinKey {
                    label: "Step".into(),
                    property: "id".into(),
                },
                QueryJoinKey {
                    label: "CompletedStep".into(),
                    property: "step_id".into(),
                },
            ],
        },
    ]
}

pub fn steps_happen_in_any_order_query() -> &'static str  {
    "
  MATCH
    (cust:Customer)-[:COMPLETES_STEP]->(compStepX:CompletedStep)-[:IS_STEP]->(:Step {name:'Step X'}),
    (cust:Customer)-[:COMPLETES_STEP]->(compStepY:CompletedStep)-[:IS_STEP]->(:Step {name:'Step Y'}),
    (cust:Customer)-[:COMPLETES_STEP]->(compStepZ:CompletedStep)-[:IS_STEP]->(:Step {name:'Step Z'})
  WITH
    cust,
    [compStepX, compStepY, compStepZ] AS completedSteps
  WITH
    cust,
    reduce ( min = 1385148606519, step IN completedSteps | 
      CASE WHEN step.timestamp < min THEN step.timestamp ELSE min END
    ) AS minTimestamp,
    reduce ( max = 0, step IN completedSteps | 
      CASE WHEN step.timestamp > max THEN step.timestamp ELSE max END
    ) AS maxTimestamp
  WHERE
    maxTimestamp - minTimestamp < 30 * 24 * 60 * 60
  RETURN
    elementId(cust) AS customerId, cust.email as customerEmail"
}
