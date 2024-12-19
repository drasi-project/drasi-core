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

use std::{
    collections::HashMap,
    ops::Deref,
    str::FromStr,
    sync::Arc,
};

use async_trait::async_trait;
use drasi_core::{
    interface::{ElementIndex, MiddlewareError, MiddlewareSetupError, SourceMiddleware, SourceMiddlewareFactory}, models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange, SourceMiddlewareConfig}
};
use jsonpath_rust::{path::config::JsonPathConfig, JsonPathInst};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnwindMapping {
    selector: JsonPathExpression,
    label: String,
    key: Option<JsonPathExpression>,
    relation: Option<String>,
}


#[derive(Clone)]
pub struct JsonPathExpression {
    expression: String,
    path: JsonPathInst,
}

impl JsonPathExpression {
    pub fn execute(&self, value: &Value) -> Vec<Value> {
        let result = self.path.find_slice(value, JsonPathConfig::default());
        result
            .into_iter()
            .map(|v| v.deref().clone())
            .collect::<Vec<Value>>()
    }

    pub fn execute_one(&self, value: &Value) -> Option<Value> {
        let result = self.path.find_slice(value, JsonPathConfig::default());
        result.first().map(|v| v.deref().clone())
    }
}

impl<'de> Deserialize<'de> for JsonPathExpression {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let expression = String::deserialize(deserializer)?;
        let path = match JsonPathInst::from_str(&expression) {
            Ok(p) => p,
            Err(e) => return Err(serde::de::Error::custom(e.to_string())),
        };
        Ok(JsonPathExpression { expression, path })
    }
}

impl std::fmt::Debug for JsonPathExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#?}", self.expression)
    }
}

pub struct Unwind {
    mappings: HashMap<String, Vec<UnwindMapping>>,
}

#[async_trait]
impl SourceMiddleware for Unwind {
    async fn process(
        &self,
        source_change: SourceChange,
        element_index: Arc<dyn ElementIndex>,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        
        let metadata = match &source_change {
            SourceChange::Insert { element } => element.get_metadata(),
            SourceChange::Update { element } => element.get_metadata(),
            SourceChange::Delete { metadata } => metadata,
            SourceChange::Future { .. } => return Ok(vec![source_change]),
        };

        let mut results = Vec::new();
        

        for label in metadata.labels.iter() {
            let mappings = match self.mappings.get(&label.to_string()) {
                Some(mapping) => mapping,
                None => continue,
            };
            
            log::info!("Processing mappings for label: {}", label);

            #[allow(unused_assignments)]
            let mut del_element = None;

            #[allow(clippy::unwrap_used)]
            let element = match &source_change {
                SourceChange::Insert { element } => element,
                SourceChange::Update { element } => element,
                SourceChange::Delete { metadata } => {
                    match element_index.get_element(&metadata.reference).await {
                        Ok(Some(element)) => {
                            del_element = Some(element);
                            del_element.as_ref().unwrap()
                        },
                        Ok(None) => continue,
                        Err(e) => {
                            return Err(MiddlewareError::IndexError(e));
                        }
                    }
                }
                _ => continue,
            };

            let source_obj: Value = element.into();

            for mapping in mappings {

                let selected = mapping.selector.execute(&source_obj);

                for (index, obj) in selected.iter().enumerate() {
                    
                    let mut new_metadata = metadata.clone();

                    let id_suffix = match &mapping.key {
                        Some(key) => match key.execute_one(&obj) {
                            Some(id) => match id.as_str() {
                                Some(id) => id.to_string(),
                                None => index.to_string(),
                            },
                            None => index.to_string(),
                        },
                        None => index.to_string(),
                    };

                    new_metadata.reference.element_id = Arc::from(format!("$unwind-{}-{}", metadata.reference.element_id, id_suffix));

                    new_metadata.labels = Arc::from(vec![Arc::from(mapping.label.clone())]);

                    let properties = match obj.as_object() {
                        Some(obj) => obj.into(),
                        None => ElementPropertyMap::new(),
                    };

                    let relation = match &mapping.relation {
                        Some(rel_label) => Some(Element::Relation {
                                metadata: ElementMetadata {
                                    reference: ElementReference::new(&new_metadata.reference.source_id, &format!("{}$rel", new_metadata.reference.element_id)),
                                    labels: Arc::new([Arc::from(rel_label.as_str())]),
                                    effective_from: new_metadata.effective_from,
                                },
                                properties: ElementPropertyMap::new(),
                                out_node: metadata.reference.clone(),
                                in_node: new_metadata.reference.clone(),
                            }),
                        None => None,
                    };

                    match &source_change {
                        SourceChange::Insert { .. } => {
                            results.push(SourceChange::Insert {
                                element: Element::Node {
                                    metadata: new_metadata,
                                    properties,
                                },
                            });
                            if let Some(rel) = relation {
                                results.push(SourceChange::Insert {
                                    element: rel,
                                });
                            }
                        }
                        SourceChange::Update { .. } => {
                            results.push(SourceChange::Update {
                                element: Element::Node {
                                    metadata: new_metadata,
                                    properties,
                                },
                            });
                            if let Some(rel) = relation {
                                results.push(SourceChange::Update {
                                    element: rel,
                                });
                            }
                        }
                        SourceChange::Delete { .. } => {
                            results.push(SourceChange::Delete { metadata: new_metadata });
                            if let Some(rel) = relation {
                                results.push(SourceChange::Delete { 
                                    metadata: rel.get_metadata().clone()
                                });
                            }
                        }
                        _ => {}
                    }

                }
            
            }
        }

        results.push(source_change);
        Ok(results)
    }
}

pub struct UnwindFactory {}

impl Default for UnwindFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl UnwindFactory {
    pub fn new() -> Self {
        UnwindFactory {}
    }
}

impl SourceMiddlewareFactory for UnwindFactory {
    fn name(&self) -> String {
        "unwind".to_string()
    }

    fn create(
        &self,
        config: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        let cfg = Value::Object(config.config.clone());
        let mappings = match serde_json::from_value(cfg) {
            Ok(mappings) => mappings,
            Err(e) => {
                return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                    "Invalid configuration: {}",
                    e
                )))
            }
        };

        log::info!(
            "Unwind middleware {} mappings loaded: {:#?}",
            config.name,
            mappings
        );

        Ok(Arc::new(Unwind { mappings }))
    }
}
