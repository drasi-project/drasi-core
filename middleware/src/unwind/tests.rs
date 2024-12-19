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

mod process {
    use std::sync::Arc;

    use drasi_core::{
        in_memory_index::in_memory_element_index::InMemoryElementIndex, interface::SourceMiddlewareFactory, models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
            SourceMiddlewareConfig,
        }
    };
    use serde_json::json;

    use crate::unwind::UnwindFactory;

    #[tokio::test]
    pub async fn unwind_array_with_relations() {
        let factory = UnwindFactory::new();
        let config = json!({
            "Pod": [{
                "selector": "$.status.containerStatuses[*]",
                "label": "Container",
                "key": "$.containerID",
                "relation": "OWNS"
            }]            
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "unwind".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let result = subject
            .process(SourceChange::Insert {
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
            }, element_index.clone())
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        println!("{:#?}", result);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0],
            SourceChange::Update {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "v1"),
                        labels: vec!["Vehicle".into()].into(),
                        effective_from: 0
                    },
                    properties: ElementPropertyMap::from(json!({
                        "id": "v1",
                        "currentSpeed": "119"
                    }))
                }
            }
        );
    }

}

mod factory {
    use drasi_core::{interface::SourceMiddlewareFactory, models::SourceMiddlewareConfig};
    use serde_json::json;

    use crate::unwind::UnwindFactory;

    #[tokio::test]
    pub async fn construct_map_middleware() {
        let subject = UnwindFactory::new();
        let config = json!({
            "Pod": [{
                "selector": "$.status.containerStatuses[*]",
                "label": "Container",
                "key": "$.containerID",
                "relation": "OWNS"
            }]            
        });

        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "unwind".into(),
            config: config.as_object().unwrap().clone(),
        };

        assert!(subject.create(&mw_config).is_ok());
    }

    #[tokio::test]
    pub async fn invalid_selector() {
        let subject = UnwindFactory::new();
        let config = json!({
            "Pod": [{
                "selector": "z$.status.containerStatuses[*]",
                "label": "Container",
                "key": "$.containerID",
                "relation": "OWNS"
            }]            
        });

        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "unwind".into(),
            config: config.as_object().unwrap().clone(),
        };

        assert!(subject.create(&mw_config).is_err());
    }
}
