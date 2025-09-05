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

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use drasi_core::{
    interface::{
        ElementIndex, MiddlewareError, MiddlewareSetupError, SourceMiddleware,
        SourceMiddlewareFactory,
    },
    models::{Element, ElementMetadata, SourceChange, SourceMiddlewareConfig},
};
use serde::Deserialize;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Deserialize)]
pub struct RelabelMiddlewareConfig {
    #[serde(rename = "labelMappings")]
    pub label_mappings: HashMap<String, String>,
}

pub struct RelabelMiddleware {
    label_mappings: HashMap<String, String>,
}

impl RelabelMiddleware {
    pub fn new(config: RelabelMiddlewareConfig) -> Self {
        RelabelMiddleware {
            label_mappings: config.label_mappings,
        }
    }

    fn relabel_element(&self, element: Element) -> Element {
        let metadata = element.get_metadata();
        let new_labels: Vec<Arc<str>> = metadata
            .labels
            .iter()
            .map(|label| {
                self.label_mappings
                    .get(label.as_ref())
                    .map(|new_label| Arc::from(new_label.as_str()))
                    .unwrap_or_else(|| label.clone())
            })
            .collect();

        let new_metadata = ElementMetadata {
            reference: metadata.reference.clone(),
            labels: Arc::from(new_labels),
            effective_from: metadata.effective_from,
        };

        match element {
            Element::Node { properties, .. } => Element::Node {
                metadata: new_metadata,
                properties,
            },
            Element::Relation {
                in_node,
                out_node,
                properties,
                ..
            } => Element::Relation {
                metadata: new_metadata,
                in_node,
                out_node,
                properties,
            },
        }
    }
}

#[async_trait]
impl SourceMiddleware for RelabelMiddleware {
    async fn process(
        &self,
        source_change: SourceChange,
        _element_index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        match source_change {
            SourceChange::Insert { element } => {
                let relabeled_element = self.relabel_element(element);
                Ok(vec![SourceChange::Insert {
                    element: relabeled_element,
                }])
            }
            SourceChange::Update { element } => {
                let relabeled_element = self.relabel_element(element);
                Ok(vec![SourceChange::Update {
                    element: relabeled_element,
                }])
            }
            SourceChange::Delete { metadata } => {
                let new_labels: Vec<Arc<str>> = metadata
                    .labels
                    .iter()
                    .map(|label| {
                        self.label_mappings
                            .get(label.as_ref())
                            .map(|new_label| Arc::from(new_label.as_str()))
                            .unwrap_or_else(|| label.clone())
                    })
                    .collect();

                let new_metadata = ElementMetadata {
                    reference: metadata.reference.clone(),
                    labels: Arc::from(new_labels),
                    effective_from: metadata.effective_from,
                };

                Ok(vec![SourceChange::Delete {
                    metadata: new_metadata,
                }])
            }
            SourceChange::Future { .. } => Ok(vec![source_change]),
        }
    }
}

pub struct RelabelMiddlewareFactory {}

impl RelabelMiddlewareFactory {
    pub fn new() -> Self {
        RelabelMiddlewareFactory {}
    }
}

impl Default for RelabelMiddlewareFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceMiddlewareFactory for RelabelMiddlewareFactory {
    fn name(&self) -> String {
        "relabel".to_string()
    }

    fn create(
        &self,
        config: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        let relabel_config: RelabelMiddlewareConfig =
            match serde_json::from_value(serde_json::Value::Object(config.config.clone())) {
                Ok(cfg) => cfg,
                Err(e) => {
                    return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                        "[{}] Invalid configuration: {}",
                        config.name, e
                    )))
                }
            };

        if relabel_config.label_mappings.is_empty() {
            return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                "[{}] At least one label mapping must be specified",
                config.name
            )));
        }

        log::info!(
            "[{}] Creating Relabel middleware with {} label mappings",
            config.name,
            relabel_config.label_mappings.len()
        );

        Ok(Arc::new(RelabelMiddleware::new(relabel_config)))
    }
}
