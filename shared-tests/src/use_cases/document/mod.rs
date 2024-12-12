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
    evaluation::{context::QueryPartEvaluationContext, variable_value::VariableValue},
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
pub async fn document(config: &(impl QueryTestConfig + Send)) {
    let rm_query = {
        let mut builder = QueryBuilder::new(queries::document_query());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    //Add initial value
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "pod-1"),
                    labels: Arc::new([Arc::from("Pod")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "metadata": {
                        "name": "pod-1",
                        "namespace": "default",
                        "labels": {
                            "app": "nginx",
                            "env": "prod"
                        },
                        "annotations": {
                            "dapr.io/enabled": "true",
                            "dapr.io/app-id": "my-app"
                        }
                    },
                    "spec": {
                        "containers": [
                            {
                                "name": "nginx",
                                "image": "nginx:latest",
                                "env": [
                                    {
                                        "name": "ENV_VAR1",
                                        "value": "value1"
                                    },
                                    {
                                        "name": "ENV_VAR2",
                                        "value": "value2"
                                    }
                                ],
                                "args": [
                                    "arg1",
                                    "arg2"
                                ]
                            }
                        ]
                    },
                    "status": {
                        "phase": "Running",
                        "conditions": [
                            {
                                "type": "Ready",
                                "status": "True"
                            }
                        ],
                        "containerStatuses": [
                            {
                                "containerID": "docker://1234567890",
                                "name": "nginx",
                                "ready": true,
                                "restartCount": 3,
                                "state": {
                                    "running": {
                                        "startedAt": "2021-01-01T00:00:00Z"
                                    }
                                }
                            },
                            {
                                "containerID": "docker://0987654321",
                                "name": "sidecar",
                                "ready": true,
                                "restartCount": 2,
                                "lastState": {
                                    "terminated": {
                                        "startedAt": "2021-01-01T00:00:00Z",
                                        "finishedAt": "2021-01-01T00:00:00Z",
                                        "exitCode": 0
                                    }
                                },
                                "state": {
                                    "terminated": {
                                        "startedAt": "2021-01-01T00:00:00Z",
                                        "finishedAt": "2021-01-01T00:00:00Z",
                                        "exitCode": 0
                                    }
                                }
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
        println!("Node Result - Add t1: {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
                "name" => VariableValue::from(json!("pod-1")),
                "namespace" => VariableValue::from(json!("default")),
                "app" => VariableValue::from(json!("nginx")),
                "app_id" => VariableValue::from(json!("my-app")),
                "container_0_image" => VariableValue::from(json!("nginx:latest")),
                "total_restart_count" => VariableValue::from(json!(5)),
                "sidecar_container_id" => VariableValue::from(json!("docker://0987654321"))
            )
        }));
    }
}
