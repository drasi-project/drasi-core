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

use std::{collections::HashMap, ops::Deref, str::FromStr, sync::Arc};

use async_trait::async_trait;
use drasi_core::{
    interface::{
        ElementIndex, MiddlewareError, MiddlewareSetupError, SourceMiddleware,
        SourceMiddlewareFactory,
    },
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        SourceMiddlewareConfig,
    },
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
        element_index: &dyn ElementIndex,
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

            let (new_element, old_element) = match &source_change {
                SourceChange::Insert { element } => (Some(element), None),
                SourceChange::Update { element } => {
                    match element_index.get_element(&metadata.reference).await {
                        Ok(old) => (Some(element), old),
                        Err(e) => {
                            return Err(MiddlewareError::IndexError(e));
                        }
                    }
                }
                SourceChange::Delete { metadata } => {
                    match element_index.get_element(&metadata.reference).await {
                        Ok(Some(element)) => (None, Some(element)),
                        Ok(None) => continue,
                        Err(e) => {
                            return Err(MiddlewareError::IndexError(e));
                        }
                    }
                }
                _ => continue,
            };

            let new_element_obj: Option<Value> = new_element.map(|x| x.into());
            let old_element_obj: Option<Value> = old_element.map(|x| x.as_ref().into());

            for mapping in mappings {
                let mut change_map = HashMap::new();

                if let Some(new_obj) = &new_element_obj {
                    let selected = mapping.selector.execute(new_obj);

                    for (index, obj) in selected.iter().enumerate() {
                        let mut new_metadata = metadata.clone();

                        let item_id = match &mapping.key {
                            Some(key) => match key.execute_one(obj) {
                                Some(id) => match id.as_str() {
                                    Some(id) => id.to_string(),
                                    None => index.to_string(),
                                },
                                None => index.to_string(),
                            },
                            None => index.to_string(),
                        };

                        let mut item_source_changes = Vec::new();

                        new_metadata.reference.element_id = Arc::from(format_item_id(
                            &mapping.selector.expression,
                            &metadata.reference.element_id,
                            &item_id,
                        ));
                        new_metadata.labels = Arc::from(vec![Arc::from(mapping.label.clone())]);

                        let properties = match obj.as_object() {
                            Some(obj) => obj.into(),
                            None => ElementPropertyMap::new(),
                        };

                        let relation = match &mapping.relation {
                            Some(rel_label) => Some(Element::Relation {
                                metadata: ElementMetadata {
                                    reference: ElementReference::new(
                                        &new_metadata.reference.source_id,
                                        &format_relation_id(&new_metadata.reference.element_id),
                                    ),
                                    labels: Arc::new([Arc::from(rel_label.as_str())]),
                                    effective_from: new_metadata.effective_from,
                                },
                                properties: ElementPropertyMap::new(),
                                out_node: new_metadata.reference.clone(),
                                in_node: metadata.reference.clone(),
                            }),
                            None => None,
                        };

                        match &source_change {
                            SourceChange::Insert { .. } => {
                                item_source_changes.push(SourceChange::Insert {
                                    element: Element::Node {
                                        metadata: new_metadata,
                                        properties,
                                    },
                                });
                                if let Some(rel) = relation {
                                    item_source_changes.push(SourceChange::Insert { element: rel });
                                }
                            }
                            SourceChange::Update { .. } => {
                                item_source_changes.push(SourceChange::Update {
                                    element: Element::Node {
                                        metadata: new_metadata,
                                        properties,
                                    },
                                });
                                if let Some(rel) = relation {
                                    item_source_changes.push(SourceChange::Update { element: rel });
                                }
                            }
                            SourceChange::Delete { .. } => {
                                item_source_changes.push(SourceChange::Delete {
                                    metadata: new_metadata,
                                });
                                if let Some(rel) = relation {
                                    item_source_changes.push(SourceChange::Delete {
                                        metadata: rel.get_metadata().clone(),
                                    });
                                }
                            }
                            _ => {}
                        }

                        change_map.insert(item_id.clone(), item_source_changes);
                    }
                }

                if let Some(old_obj) = &old_element_obj {
                    let selected = mapping.selector.execute(old_obj);

                    for (index, obj) in selected.iter().enumerate() {
                        let item_id = match &mapping.key {
                            Some(key) => match key.execute_one(obj) {
                                Some(id) => match id.as_str() {
                                    Some(id) => id.to_string(),
                                    None => index.to_string(),
                                },
                                None => index.to_string(),
                            },
                            None => index.to_string(),
                        };

                        if change_map.contains_key(&item_id) {
                            continue;
                        }

                        let mut item_source_changes = Vec::new();

                        let child_id = format_item_id(
                            &mapping.selector.expression,
                            &metadata.reference.element_id,
                            &item_id,
                        );

                        item_source_changes.push(SourceChange::Delete {
                            metadata: ElementMetadata {
                                reference: ElementReference::new(
                                    &metadata.reference.source_id,
                                    &child_id,
                                ),
                                labels: Arc::new([Arc::from(mapping.label.clone())]),
                                effective_from: metadata.effective_from,
                            },
                        });

                        if let Some(rel_label) = &mapping.relation {
                            item_source_changes.push(SourceChange::Delete {
                                metadata: ElementMetadata {
                                    reference: ElementReference::new(
                                        &metadata.reference.source_id,
                                        &format_relation_id(&child_id),
                                    ),
                                    labels: Arc::new([Arc::from(rel_label.as_str())]),
                                    effective_from: metadata.effective_from,
                                },
                            });
                        }

                        change_map.insert(item_id.clone(), item_source_changes);
                    }
                }

                for (_, changes) in change_map {
                    results.extend(changes);
                }
            }
        }

        results.push(source_change);
        Ok(results)
    }
}

fn format_item_id(expression: &str, parent_id: &str, item_id: &str) -> String {
    format!("$unwind-{}-{}-{}", expression, parent_id, item_id)
}

fn format_relation_id(child_id: &str) -> String {
    format!("{}$rel", child_id)
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
