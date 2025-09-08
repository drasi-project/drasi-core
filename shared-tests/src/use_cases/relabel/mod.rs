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

use drasi_middleware::relabel::RelabelMiddlewareFactory;
use serde_json::json;

use drasi_core::{
    evaluation::{
        context::QueryPartEvaluationContext, functions::FunctionRegistry,
        variable_value::VariableValue,
    },
    middleware::MiddlewareTypeRegistry,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;

use crate::QueryTestConfig;

mod queries;

macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into(), $val); )*
       map
  }}
}

#[allow(clippy::print_stdout, clippy::unwrap_used)]
pub async fn relabel(config: &(impl QueryTestConfig + Send)) {
    let mut middleware_registry = MiddlewareTypeRegistry::new();
    middleware_registry.register(Arc::new(RelabelMiddlewareFactory::new()));
    let middleware_registry = Arc::new(middleware_registry);

    let rm_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::relabel_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry);
        for mw in queries::middlewares() {
            builder = builder.with_source_middleware(mw);
        }
        builder = builder.with_source_pipeline("test", &queries::source_pipeline());
        builder.build().await
    };

    // Insert a Person node (should be relabeled to User)
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "person1"),
                    labels: vec!["Person".into()].into(),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "name": "John Doe",
                    "email": "john.doe@example.com",
                    "role": "Developer"
                })),
            },
        };

        let result = rm_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        println!("Node Result - Add Person (should be User): {result:?}");
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
                "userName" => VariableValue::from(json!("John Doe")),
                "userEmail" => VariableValue::from(json!("john.doe@example.com")),
                "userRole" => VariableValue::from(json!("Developer"))
            )
        }));
    }

    // Insert an Employee node (should be relabeled to Staff)
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "employee1"),
                    labels: vec!["Employee".into()].into(),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "name": "Jane Smith",
                    "email": "jane.smith@example.com",
                    "role": "Manager"
                })),
            },
        };

        // Employee gets relabeled to Staff, but our query looks for User, so no results
        let result = rm_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
        println!("Node Result - Add Employee (should be Staff, no match): {result:?}");
    }

    // Update the Person node
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "person1"),
                    labels: vec!["Person".into()].into(),
                    effective_from: 1,
                },
                properties: ElementPropertyMap::from(json!({
                    "name": "John Doe",
                    "email": "john.doe@company.com",
                    "role": "Senior Developer"
                })),
            },
        };

        let result = rm_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        println!("Node Result - Update Person: {result:?}");
        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
                "userName" => VariableValue::from(json!("John Doe")),
                "userEmail" => VariableValue::from(json!("john.doe@example.com")),
                "userRole" => VariableValue::from(json!("Developer"))
            ),
            after: variablemap!(
                "userName" => VariableValue::from(json!("John Doe")),
                "userEmail" => VariableValue::from(json!("john.doe@company.com")),
                "userRole" => VariableValue::from(json!("Senior Developer"))
            )
        }));
    }

    // Insert a node with multiple labels (Person and Employee)
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "multi1"),
                    labels: vec!["Person".into(), "Employee".into()].into(),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "name": "Alice Johnson",
                    "email": "alice@example.com",
                    "role": "Lead"
                })),
            },
        };

        // Both Person->User and Employee->Staff, but query looks for User, so it matches
        let result = rm_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        println!("Node Result - Add Person+Employee (becomes User+Staff): {result:?}");
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
                "userName" => VariableValue::from(json!("Alice Johnson")),
                "userEmail" => VariableValue::from(json!("alice@example.com")),
                "userRole" => VariableValue::from(json!("Lead"))
            )
        }));
    }

    // Insert a Company node (should be relabeled to Organization)
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "company1"),
                    labels: vec!["Company".into()].into(),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "name": "Tech Corp",
                    "email": "info@techcorp.com",
                    "role": "Business"
                })),
            },
        };

        // Company gets relabeled to Organization, but our query looks for User, so no results
        let result = rm_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
        println!("Node Result - Add Company (should be Organization, no match): {result:?}");
    }

    // Delete the Person node
    {
        let change = SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "person1"),
                labels: vec!["Person".into()].into(),
                effective_from: 2,
            },
        };

        let result = rm_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        println!("Node Result - Delete Person: {result:?}");
        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
                "userName" => VariableValue::from(json!("John Doe")),
                "userEmail" => VariableValue::from(json!("john.doe@company.com")),
                "userRole" => VariableValue::from(json!("Senior Developer"))
            )
        }));
    }

    // Insert a node with an unmapped label (should remain unchanged)
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "unmapped1"),
                    labels: vec!["UnmappedLabel".into()].into(),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "name": "Unmapped Node",
                    "email": "unmapped@example.com",
                    "role": "Unknown"
                })),
            },
        };

        // UnmappedLabel stays as UnmappedLabel, query looks for User, so no results
        let result = rm_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
        println!("Node Result - Add UnmappedLabel (no change, no match): {result:?}");
    }
}
