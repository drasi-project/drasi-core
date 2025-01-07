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

use drasi_middleware::unwind::UnwindFactory;
use serde_json::json;

use drasi_core::{
    evaluation::{context::QueryPartEvaluationContext, variable_value::VariableValue},
    middleware::MiddlewareTypeRegistry,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};

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
pub async fn unwind(config: &(impl QueryTestConfig + Send)) {
    let mut middleware_registry = MiddlewareTypeRegistry::new();
    middleware_registry.register(Arc::new(UnwindFactory::new()));
    let middleware_registry = Arc::new(middleware_registry);

    let rm_query = {
        let mut builder = QueryBuilder::new(queries::unwind_query());
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry);
        for mw in queries::middlewares() {
            builder = builder.with_source_middleware(mw);
        }
        builder = builder.with_source_pipeline("test", &queries::source_pipeline());
        builder.build().await
    };

    //Add initial value
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p1"),
                    labels: vec!["Pod".into()].into(),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "metadata": {
                        "name": "pod-1",
                        "namespace": "default",
                        "labels": {
                            "app": "nginx",
                            "env": "prod"
                        }
                    },
                    "status": {
                        "containerStatuses": [
                            {
                                "containerID": "c1",
                                "name": "nginx"
                            },
                            {
                                "containerID": "c2",
                                "name": "redis"
                            }
                        ]
                    }
                })),
            },
        };

        let result = rm_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 2);
        println!("Node Result - Add p1: {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
                "pod" => VariableValue::from(json!("pod-1")),
                "containerID" => VariableValue::from(json!("c1")),
                "name" => VariableValue::from(json!("nginx"))
            )
        }));
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
                "pod" => VariableValue::from(json!("pod-1")),
                "containerID" => VariableValue::from(json!("c2")),
                "name" => VariableValue::from(json!("redis"))
            )
        }));
    }

    //Add additional container
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p1"),
                    labels: vec!["Pod".into()].into(),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "metadata": {
                        "name": "pod-1",
                        "namespace": "default",
                        "labels": {
                            "app": "nginx",
                            "env": "prod"
                        }
                    },
                    "status": {
                        "containerStatuses": [
                            {
                                "containerID": "c1",
                                "name": "nginx"
                            },
                            {
                                "containerID": "c2",
                                "name": "redis"
                            },
                            {
                                "containerID": "c3",
                                "name": "dapr"
                            }
                        ]
                    }
                })),
            },
        };

        let result = rm_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        println!("Node Result - Update p1: {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
                "pod" => VariableValue::from(json!("pod-1")),
                "containerID" => VariableValue::from(json!("c3")),
                "name" => VariableValue::from(json!("dapr"))
            )
        }));
    }

    //remove container / update container
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p1"),
                    labels: vec!["Pod".into()].into(),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "metadata": {
                        "name": "pod-1",
                        "namespace": "default",
                        "labels": {
                            "app": "nginx",
                            "env": "prod"
                        }
                    },
                    "status": {
                        "containerStatuses": [
                            {
                                "containerID": "c1",
                                "name": "nginx2"
                            },
                            {
                                "containerID": "c3",
                                "name": "dapr"
                            }
                        ]
                    }
                })),
            },
        };

        let result = rm_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        println!("Node Result - Update p1: {:?}", result);
        assert_eq!(result.len(), 2);

        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
                "pod" => VariableValue::from(json!("pod-1")),
                "containerID" => VariableValue::from(json!("c2")),
                "name" => VariableValue::from(json!("redis"))
            )
        }));

        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
                "pod" => VariableValue::from(json!("pod-1")),
                "containerID" => VariableValue::from(json!("c1")),
                "name" => VariableValue::from(json!("nginx"))
            ),
            after: variablemap!(
                "pod" => VariableValue::from(json!("pod-1")),
                "containerID" => VariableValue::from(json!("c1")),
                "name" => VariableValue::from(json!("nginx2"))
            )
        }));
    }

    //remove pod
    {
        let change = SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "p1"),
                labels: vec!["Pod".into()].into(),
                effective_from: 0,
            },
        };

        let result = rm_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        println!("Node Result - Delete p1: {:?}", result);
        assert_eq!(result.len(), 2);

        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
                "pod" => VariableValue::from(json!("pod-1")),
                "containerID" => VariableValue::from(json!("c1")),
                "name" => VariableValue::from(json!("nginx2"))
            )
        }));

        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
                "pod" => VariableValue::from(json!("pod-1")),
                "containerID" => VariableValue::from(json!("c3")),
                "name" => VariableValue::from(json!("dapr"))
            )
        }));
    }
}
