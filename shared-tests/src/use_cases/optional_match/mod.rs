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

#![allow(clippy::unwrap_used)]
use std::{collections::BTreeMap, sync::Arc};

use serde_json::json;

use drasi_core::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::FunctionRegistry,
        variable_value::VariableValue,
    },
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, QueryJoin, QueryJoinKey,
        SourceChange,
    },
    query::QueryBuilder,
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;

use super::{contains_data, IGNORED_ROW_SIGNATURE};
use crate::QueryTestConfig;

mod queries;

macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into(), $val); )*
       map
  }}
}

fn assert_replacement(
    result: &[QueryPartEvaluationContext],
    before: QueryVariables,
    after: QueryVariables,
) -> (u64, u64) {
    let [QueryPartEvaluationContext::Removing {
        before: actual_before,
        row_signature: old,
    }, QueryPartEvaluationContext::Adding {
        after: actual_after,
        row_signature: new,
    }] = result
    else {
        panic!("a changed MATCH identity requires removal followed by addition: {result:?}");
    };
    assert_eq!(actual_before, &before);
    assert_eq!(actual_after, &after);
    assert_ne!(old, new);
    (*old, *new)
}

pub async fn optional_match(config: &(impl QueryTestConfig + Send)) {
    let opt_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::optional_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };
    let unmatched_signature;
    let first_signature;
    let second_signature;

    //Add invoice 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "amount": 100,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        unmatched_signature = result[0].row_signature();
        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                  "amount" => VariableValue::from(json!(100.0)),
                  "id" => VariableValue::from(json!("i1")),
                  "payment_amount" => VariableValue::Null
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //Add relation 1
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r1"),
                    labels: Arc::new([Arc::from("RECONCILED_TO")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "i1"),
                out_node: ElementReference::new("test", "p1"),
                properties: ElementPropertyMap::from(json!({
                    "id": "r1",
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //Add payment 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p1"),
                    labels: Arc::new([Arc::from("Payment")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "p1",
                    "amount": 70,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        let (removed, added) = assert_replacement(
            &result,
            variablemap!(
              "amount" => VariableValue::from(json!(100.0)),
              "id" => VariableValue::from(json!("i1")),
              "payment_amount" => VariableValue::Null
            ),
            variablemap!(
              "amount" => VariableValue::from(json!(100.0)),
              "id" => VariableValue::from(json!("i1")),
              "payment_amount" => VariableValue::from(json!(70.0))
            ),
        );
        assert_eq!(removed, unmatched_signature);
        first_signature = added;
    }

    //Add relation 2
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r2"),
                    labels: Arc::new([Arc::from("RECONCILED_TO")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "i1"),
                out_node: ElementReference::new("test", "p2"),
                properties: ElementPropertyMap::from(json!({
                    "id": "r2",
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //Add payment 2
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p2"),
                    labels: Arc::new([Arc::from("Payment")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "p2",
                    "amount": 10,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        second_signature = result[0].row_signature();
        assert_ne!(second_signature, first_signature);
        assert_ne!(second_signature, unmatched_signature);
        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                  "amount" => VariableValue::from(json!(100.0)),
                  "id" => VariableValue::from(json!("i1")),
                  "payment_amount" => VariableValue::from(json!(10.0))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //update payment 2
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p2"),
                    labels: Arc::new([Arc::from("Payment")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "p2",
                    "amount": 15,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].row_signature(), second_signature);
        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Updating {
                before: variablemap!(
                  "amount" => VariableValue::from(json!(100.0)),
                  "id" => VariableValue::from(json!("i1")),
                    "payment_amount" => VariableValue::from(json!(10.0))
                ),
                after: variablemap!(
                  "amount" => VariableValue::from(json!(100.0)),
                  "id" => VariableValue::from(json!("i1")),
                  "payment_amount" => VariableValue::from(json!(15.0))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //delete payment 2
    {
        let change = SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "p2"),
                labels: Arc::new([Arc::from("Payment")]),
                effective_from: 0,
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].row_signature(), second_signature);
        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Removing {
                before: variablemap!(
                  "amount" => VariableValue::from(json!(100.0)),
                  "id" => VariableValue::from(json!("i1")),
                    "payment_amount" => VariableValue::from(json!(15.0))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }
}

pub async fn optional_match_aggregating(config: &(impl QueryTestConfig + Send)) {
    let opt_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::optional_query_aggregating(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    //Add invoice 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "amount": 100,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                grouping_keys: vec!["id".to_string(), "amount".to_string()],
                default_before: true,
                default_after: false,
                before: Some(variablemap!(
                    "amount" => VariableValue::from(json!(100.0)),
                    "balance" => VariableValue::from(json!(100.0)),
                    "payment_amount" => VariableValue::from(json!(0.0)),
                    "id" => VariableValue::from(json!("i1"))

                )),
                after: variablemap!(
                    "amount" => VariableValue::from(json!(100.0)),
                    "balance" => VariableValue::from(json!(100.0)),
                    "payment_amount" => VariableValue::from(json!(0.0)),
                    "id" => VariableValue::from(json!("i1"))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //Add relation 1
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r1"),
                    labels: Arc::new([Arc::from("RECONCILED_TO")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "i1"),
                out_node: ElementReference::new("test", "p1"),
                properties: ElementPropertyMap::from(json!({
                    "id": "r1",
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //Add payment 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p1"),
                    labels: Arc::new([Arc::from("Payment")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "p1",
                    "amount": 70,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                grouping_keys: vec!["id".to_string(), "amount".to_string()],
                default_before: false,
                default_after: false,
                before: Some(variablemap!(
                    "amount" => VariableValue::from(json!(100.0)),
                    "balance" => VariableValue::from(json!(100.0)),
                    "payment_amount" => VariableValue::from(json!(0.0)),
                    "id" => VariableValue::from(json!("i1"))

                )),
                after: variablemap!(
                    "amount" => VariableValue::from(json!(100.0)),
                    "balance" => VariableValue::from(json!(30.0)),
                    "payment_amount" => VariableValue::from(json!(70.0)),
                    "id" => VariableValue::from(json!("i1"))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //Add relation 2
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r2"),
                    labels: Arc::new([Arc::from("RECONCILED_TO")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "i1"),
                out_node: ElementReference::new("test", "p2"),
                properties: ElementPropertyMap::from(json!({
                    "id": "r2",
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //Add payment 2
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p2"),
                    labels: Arc::new([Arc::from("Payment")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "p2",
                    "amount": 30,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                grouping_keys: vec!["id".to_string(), "amount".to_string()],
                default_before: true,
                default_after: false,
                before: Some(variablemap!(
                    "amount" => VariableValue::from(json!(100.0)),
                    "balance" => VariableValue::from(json!(30.0)),
                    "payment_amount" => VariableValue::from(json!(70.0)),
                    "id" => VariableValue::from(json!("i1"))

                )),
                after: variablemap!(
                    "amount" => VariableValue::from(json!(100.0)),
                    "balance" => VariableValue::from(json!(0.0)),
                    "payment_amount" => VariableValue::from(json!(100.0)),
                    "id" => VariableValue::from(json!("i1"))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }
}

pub async fn multi_optional_match(config: &(impl QueryTestConfig + Send)) {
    let opt_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::multi_optional_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };
    let unmatched_signature;
    let invoice_signature;

    //Add customer 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "c1"),
                    labels: Arc::new([Arc::from("Customer")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "c1",
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        unmatched_signature = result[0].row_signature();
        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "customer_id" => VariableValue::from(json!("c1")),
                    "invoice_id" => VariableValue::Null,
                    "payment_id" => VariableValue::Null
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //Add has 1
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "h1"),
                    labels: Arc::new([Arc::from("HAS")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "c1"),
                out_node: ElementReference::new("test", "i1"),
                properties: ElementPropertyMap::from(json!({
                    "id": "h1",
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //Add invoice 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "amount": 100,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        let (removed, added) = assert_replacement(
            &result,
            variablemap!(
                "customer_id" => VariableValue::from(json!("c1")),
                "invoice_id" => VariableValue::Null,
                "payment_id" => VariableValue::Null
            ),
            variablemap!(
                "customer_id" => VariableValue::from(json!("c1")),
                "invoice_id" => VariableValue::from(json!("i1")),
                "payment_id" => VariableValue::Null
            ),
        );
        assert_eq!(removed, unmatched_signature);
        invoice_signature = added;
    }

    //Add relation 1
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r1"),
                    labels: Arc::new([Arc::from("RECONCILED_TO")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "i1"),
                out_node: ElementReference::new("test", "p1"),
                properties: ElementPropertyMap::from(json!({
                    "id": "r1",
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //Add payment 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p1"),
                    labels: Arc::new([Arc::from("Payment")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "p1",
                    "amount": 70,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        let (removed, _) = assert_replacement(
            &result,
            variablemap!(
                "customer_id" => VariableValue::from(json!("c1")),
                "invoice_id" => VariableValue::from(json!("i1")),
                "payment_id" => VariableValue::Null
            ),
            variablemap!(
                "customer_id" => VariableValue::from(json!("c1")),
                "invoice_id" => VariableValue::from(json!("i1")),
                "payment_id" => VariableValue::from(json!("p1"))
            ),
        );
        assert_eq!(removed, invoice_signature);
    }
}

pub async fn optional_match_identity_and_join_key_changes(config: &(impl QueryTestConfig + Send)) {
    let functions = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let query = config.config_query(QueryBuilder::new(
        "MATCH (d:DecisionExplanation) OPTIONAL MATCH (d)-[:DECISION_WRITE]->(w:PlanWriteOutcome) OPTIONAL MATCH (d)-[:DECISION_POLICY]->(p:PolicyEvaluation) RETURN d.decision_id AS decision_id, w.outcome AS outcome, p.allowed AS allowed",
        Arc::new(CypherParser::new(functions.clone())),
    ).with_function_registry(functions).with_joins(
        [("DECISION_WRITE", "PlanWriteOutcome"), ("DECISION_POLICY", "PolicyEvaluation")].into_iter()
            .map(|(id, right)| QueryJoin {
                id: id.into(), keys: ["DecisionExplanation", right].into_iter()
                    .map(|label| QueryJoinKey { label: label.into(), property: "decision_id".into() }).collect(),
            }).collect(),
    )).await.build().await;
    let insert = |id, label, properties| SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", id),
                labels: Arc::from([Arc::from(label)]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::from(properties),
        },
    };
    let update = |id, properties| SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", id),
                labels: Arc::from([Arc::from("PlanWriteOutcome")]),
                effective_from: 2000,
            },
            properties: ElementPropertyMap::from(properties),
        },
    };
    let delete = |id| SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", id),
            labels: Arc::from([]),
            effective_from: 3000,
        },
    };
    let null_a = json!({"decision_id":"a","outcome":null,"allowed":null});
    let null_b = json!({"decision_id":"b","outcome":null,"allowed":null});
    let rejected_a = json!({"decision_id":"a","outcome":"rejected","allowed":null});
    let allowed_a = json!({"decision_id":"a","outcome":"rejected","allowed":true});
    let accepted_b = json!({"decision_id":"b","outcome":"accepted","allowed":null});
    let mut rows = BTreeMap::new();
    for (change, mut expected) in [
        (
            insert("a", "DecisionExplanation", json!({"decision_id":"a"})),
            vec![null_a.clone()],
        ),
        (
            insert("b", "DecisionExplanation", json!({"decision_id":"b"})),
            vec![null_a.clone(), null_b.clone()],
        ),
        (
            insert(
                "first",
                "PlanWriteOutcome",
                json!({"decision_id":"a","outcome":"rejected"}),
            ),
            vec![rejected_a.clone(), null_b.clone()],
        ),
        (
            insert(
                "second",
                "PlanWriteOutcome",
                json!({"decision_id":"a","outcome":"rejected"}),
            ),
            vec![rejected_a.clone(), rejected_a, null_b.clone()],
        ),
        (
            insert(
                "policy",
                "PolicyEvaluation",
                json!({"decision_id":"a","allowed":true}),
            ),
            vec![allowed_a.clone(), allowed_a.clone(), null_b.clone()],
        ),
        (
            update("first", json!({"decision_id":"b","outcome":"accepted"})),
            vec![allowed_a, accepted_b.clone()],
        ),
        (
            delete("second"),
            vec![
                json!({"decision_id":"a","outcome":null,"allowed":true}),
                accepted_b.clone(),
            ],
        ),
        (delete("policy"), vec![null_a.clone(), accepted_b]),
        (
            update("first", json!({"decision_id":"b","outcome":"rejected"})),
            vec![
                null_a.clone(),
                json!({"decision_id":"b","outcome":"rejected","allowed":null}),
            ],
        ),
        (delete("first"), vec![null_a, null_b.clone()]),
        (delete("a"), vec![null_b]),
        (delete("b"), Vec::new()),
    ] {
        for delta in query.process_source_change(change).await.unwrap() {
            match delta {
                QueryPartEvaluationContext::Adding {
                    after,
                    row_signature,
                } => {
                    assert!(
                        rows.insert(row_signature, after).is_none(),
                        "duplicate row identity"
                    );
                }
                QueryPartEvaluationContext::Updating {
                    before,
                    after,
                    row_signature,
                } => {
                    assert_eq!(
                        rows.get(&row_signature),
                        Some(&before),
                        "update must address an existing row identity"
                    );
                    rows.insert(row_signature, after);
                }
                QueryPartEvaluationContext::Removing {
                    before,
                    row_signature,
                } => {
                    assert_eq!(
                        rows.remove(&row_signature),
                        Some(before),
                        "removal must address an existing row identity"
                    );
                }
                other => panic!("unexpected non-aggregate result {other:?}"),
            }
        }
        let mut actual: Vec<_> = rows
            .values()
            .map(|row| serde_json::to_value(row).unwrap())
            .collect();
        actual.sort_by_key(ToString::to_string);
        expected.sort_by_key(ToString::to_string);
        assert_eq!(actual, expected);
    }
}
