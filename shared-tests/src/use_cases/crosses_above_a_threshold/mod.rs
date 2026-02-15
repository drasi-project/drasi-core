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

use std::sync::Arc;

use serde_json::json;

use drasi_core::{
    evaluation::{
        context::QueryPartEvaluationContext,
        functions::FunctionRegistry,
        variable_value::{duration::Duration, VariableValue},
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;

use self::data::get_bootstrap_data;
use crate::QueryTestConfig;

mod data;
mod queries;

macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into(), $val); )*
       map
  }}
}

async fn bootstrap_query(query: &ContinuousQuery) {
    let data = get_bootstrap_data();

    for change in data {
        let _ = query.process_source_change(change).await;
    }
}

// Query identifies when an invoice is overdue by 10 or more days. The output includes
// the number of days overdue, which results in the result being updated each time the
// number of days overdue changes.
pub async fn crosses_above_a_threshold(config: &(impl QueryTestConfig + Send)) {
    let crosses_above_a_threshold_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::crosses_above_a_threshold_query(), parser)
            .with_function_registry(function_registry)
            .with_joins(queries::crosses_above_a_threshold_metadata());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    // Add initial values
    bootstrap_query(&crosses_above_a_threshold_query).await;

    // Make invoice overdue by 1 day
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "invoice_status_invoice_01"),
                    labels: Arc::new([Arc::from("InvoiceStatus")]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "invoice_id": "invoice_01", "timestamp": 1696204800, "status": "overdue" }),
                ), // Invoice Status Date: 2023-10-02
            },
        };

        let result = crosses_above_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Result - Invoice Status 1 day overdue: {:?}", result);
        assert_eq!(result.len(), 0);
    }

    // Make invoice overdue by 5 days
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "invoice_status_invoice_01"),
                    labels: Arc::new([Arc::from("InvoiceStatus")]),
                    effective_from: 2000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "invoice_id": "invoice_01", "timestamp": 1696550400, "status": "overdue" }),
                ), // Invoice Status Date: 2023-10-06
            },
        };

        let result = crosses_above_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Result - Invoice Status 5 days overdue: {:?}", result);
        assert_eq!(result.len(), 0);
    }

    // Make invoice overdue by 9 days
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "invoice_status_invoice_01"),
                    labels: Arc::new([Arc::from("InvoiceStatus")]),
                    effective_from: 3000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "invoice_id": "invoice_01", "timestamp": 1696896000, "status": "overdue" }),
                ), // Invoice Status Date: 2023-10-10
            },
        };

        let result = crosses_above_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Result - Invoice Status 9 days overdue: {:?}", result);
        assert_eq!(result.len(), 0);
    }

    // Make invoice overdue by 10 days
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "invoice_status_invoice_01"),
                    labels: Arc::new([Arc::from("InvoiceStatus")]),
                    effective_from: 3000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "invoice_id": "invoice_01", "timestamp": 1696982400, "status": "overdue" }),
                ), // Invoice Status Date: 2023-10-11
            },
        };

        let result = crosses_above_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Result - Invoice Status 10 days overdue: {:?}", result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "accountManagerName" => VariableValue::from(json!("Employee 01")),
              "accountManagerEmail" => VariableValue::from(json!("emp_01@reflex.com")),
              "customerName" => VariableValue::from(json!("Customer 01")),
              "invoiceNumber" => VariableValue::from(json!("invoice_01"))
            )
        }));
    }

    // Make invoice overdue by 11 days
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "invoice_status_invoice_01"),
                    labels: Arc::new([Arc::from("InvoiceStatus")]),
                    effective_from: 4000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "invoice_id": "invoice_01", "timestamp": 1697068800, "status": "overdue" }),
                ), // Invoice Status Date: 2023-10-12
            },
        };

        let result = crosses_above_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Result - Invoice Status 11 days overdue: {:?}", result);
        assert_eq!(result.len(), 0);
    }

    // Make invoice overdue by 15 days
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "invoice_status_invoice_01"),
                    labels: Arc::new([Arc::from("InvoiceStatus")]),
                    effective_from: 5000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "invoice_id": "invoice_01", "timestamp": 1697414400, "status": "overdue" }),
                ), // Invoice Status Date: 2023-10-16
            },
        };

        let result = crosses_above_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Result - Invoice Status 15 days overdue: {:?}", result);
        assert_eq!(result.len(), 0);
    }

    // Make invoice PAID after 16 days
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "invoice_status_invoice_01"),
                    labels: Arc::new([Arc::from("InvoiceStatus")]),
                    effective_from: 5000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "invoice_id": "invoice_01", "timestamp": 1697500800, "status": "paid" }),
                ), // Invoice Status Date: 2023-10-17
            },
        };

        let result = crosses_above_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Result - Invoice Status PAID after 16 days: {:?}", result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "accountManagerName" => VariableValue::from(json!("Employee 01")),
              "accountManagerEmail" => VariableValue::from(json!("emp_01@reflex.com")),
              "customerName" => VariableValue::from(json!("Customer 01")),
              "invoiceNumber" => VariableValue::from(json!("invoice_01"))
            )
        }));
    }
}

// Query identifies when an invoice is overdue by 10 or more days. But the output does not include
// the number of days overdue, so the result is only updated when the invoice first becomes overdue
// by 10 days and when it is no longer overdue.
pub async fn crosses_above_a_threshold_with_overdue_days(config: &(impl QueryTestConfig + Send)) {
    let crosses_above_a_threshold_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(
            queries::crosses_above_a_threshold_with_overduedays_query(),
            parser,
        )
        .with_function_registry(function_registry)
        .with_joins(queries::crosses_above_a_threshold_metadata());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    // Add initial values
    bootstrap_query(&crosses_above_a_threshold_query).await;

    // Make invoice overdue by 1 day
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "invoice_status_invoice_01"),
                    labels: Arc::new([Arc::from("InvoiceStatus")]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "invoice_id": "invoice_01", "timestamp": 1696204800, "status": "overdue" }),
                ), // Invoice Status Date: 2023-10-02
            },
        };

        let result = crosses_above_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Result - Invoice Status 1 day overdue: {:?}", result);
        assert_eq!(result.len(), 0);
    }

    // Make invoice overdue by 5 days
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "invoice_status_invoice_01"),
                    labels: Arc::new([Arc::from("InvoiceStatus")]),
                    effective_from: 2000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "invoice_id": "invoice_01", "timestamp": 1696550400, "status": "overdue" }),
                ), // Invoice Status Date: 2023-10-06
            },
        };

        let result = crosses_above_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Result - Invoice Status 5 days overdue: {:?}", result);
        assert_eq!(result.len(), 0);
    }

    // Make invoice overdue by 9 days
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "invoice_status_invoice_01"),
                    labels: Arc::new([Arc::from("InvoiceStatus")]),
                    effective_from: 3000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "invoice_id": "invoice_01", "timestamp": 1696896000, "status": "overdue" }),
                ), // Invoice Status Date: 2023-10-10
            },
        };

        let result = crosses_above_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Result - Invoice Status 9 days overdue: {:?}", result);
        assert_eq!(result.len(), 0);
    }

    // Make invoice overdue by 10 days
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "invoice_status_invoice_01"),
                    labels: Arc::new([Arc::from("InvoiceStatus")]),
                    effective_from: 3000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "invoice_id": "invoice_01", "timestamp": 1696982400, "status": "overdue" }),
                ), // Invoice Status Date: 2023-10-11
            },
        };

        let result = crosses_above_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Result - Invoice Status 10 days overdue: {:?}", result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Adding { after: variablemap!(
        "accountManagerName" => VariableValue::from(json!("Employee 01")),
        "accountManagerEmail" => VariableValue::from(json!("emp_01@reflex.com")),
        "customerName" => VariableValue::from(json!("Customer 01")),
        "invoiceNumber" => VariableValue::from(json!("invoice_01")),
        "overdueDays" => VariableValue::Duration(Duration::new(chrono::Duration::days(10), 0, 0))
      )}));
    }

    // Make invoice overdue by 11 days
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "invoice_status_invoice_01"),
                    labels: Arc::new([Arc::from("InvoiceStatus")]),
                    effective_from: 4000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "invoice_id": "invoice_01", "timestamp": 1697068800, "status": "overdue" }),
                ), // Invoice Status Date: 2023-10-12
            },
        };

        let result = crosses_above_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Result - Invoice Status 11 days overdue: {:?}", result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Updating { before: variablemap!(
        "accountManagerName" => VariableValue::from(json!("Employee 01")),
        "accountManagerEmail" => VariableValue::from(json!("emp_01@reflex.com")),
        "customerName" => VariableValue::from(json!("Customer 01")),
        "invoiceNumber" => VariableValue::from(json!("invoice_01")),
        "overdueDays" => VariableValue::Duration(Duration::new(chrono::Duration::days(10), 0, 0))
      ),
      after: variablemap!(
        "accountManagerName" => VariableValue::from(json!("Employee 01")),
        "accountManagerEmail" => VariableValue::from(json!("emp_01@reflex.com")),
        "customerName" => VariableValue::from(json!("Customer 01")),
        "invoiceNumber" => VariableValue::from(json!("invoice_01")),
        "overdueDays" => VariableValue::Duration(Duration::new(chrono::Duration::days(11), 0, 0))
    )}));
    }

    // Make invoice overdue by 15 days
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "invoice_status_invoice_01"),
                    labels: Arc::new([Arc::from("InvoiceStatus")]),
                    effective_from: 5000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "invoice_id": "invoice_01", "timestamp": 1697414400, "status": "overdue" }),
                ), // Invoice Status Date: 2023-10-16
            },
        };

        let result = crosses_above_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Result - Invoice Status 15 days overdue: {:?}", result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Updating { before: variablemap!(
      "accountManagerName" => VariableValue::from(json!("Employee 01")),
      "accountManagerEmail" => VariableValue::from(json!("emp_01@reflex.com")),
      "customerName" => VariableValue::from(json!("Customer 01")),
      "invoiceNumber" => VariableValue::from(json!("invoice_01")),
      "overdueDays" => VariableValue::Duration(Duration::new(chrono::Duration::days(11), 0, 0))
    ),
    after: variablemap!(
      "accountManagerName" => VariableValue::from(json!("Employee 01")),
      "accountManagerEmail" => VariableValue::from(json!("emp_01@reflex.com")),
      "customerName" => VariableValue::from(json!("Customer 01")),
      "invoiceNumber" => VariableValue::from(json!("invoice_01")),
      "overdueDays" => VariableValue::Duration(Duration::new(chrono::Duration::days(15), 0, 0))
    )}));
    }

    // Make invoice PAID after 16 days
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "invoice_status_invoice_01"),
                    labels: Arc::new([Arc::from("InvoiceStatus")]),
                    effective_from: 5000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "invoice_id": "invoice_01", "timestamp": 1697500800, "status": "paid" }),
                ), // Invoice Status Date: 2023-10-17
            },
        };

        let result = crosses_above_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Result - Invoice Status PAID after 16 days: {:?}", result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Removing { before: variablemap!(
      "accountManagerName" => VariableValue::from(json!("Employee 01")),
      "accountManagerEmail" => VariableValue::from(json!("emp_01@reflex.com")),
      "customerName" => VariableValue::from(json!("Customer 01")),
      "invoiceNumber" => VariableValue::from(json!("invoice_01")),
      "overdueDays" => VariableValue::Duration(Duration::new(chrono::Duration::days(15), 0, 0))
    )}));
    }
}
