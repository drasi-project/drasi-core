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

#[cfg(feature = "engine-adapter")]
use async_trait::async_trait;
#[cfg(feature = "engine-adapter")]
use drasi_core::interface::{ElementIndex, SourceMiddleware, SourceMiddlewareFactory};
use drasi_core_types::{
    interface::{MiddlewareError, MiddlewareSetupError},
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

impl RelabelMiddleware {
    pub async fn apply(
        &self,
        source_change: SourceChange,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        match source_change {
            SourceChange::Insert { element } => {
                let relabeled_element = self.relabel_element(element);
                Ok(vec![SourceChange::Insert {
                    element: relabeled_element,
                }])
            }
            SourceChange::Replace { element } => Ok(vec![SourceChange::Replace {
                element: self.relabel_element(element),
            }]),
            SourceChange::Update {
                element,
                removed_properties,
            } => {
                let relabeled_element = self.relabel_element(element);
                Ok(vec![SourceChange::Update {
                    element: relabeled_element,
                    removed_properties,
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

impl RelabelMiddlewareFactory {
    pub fn name(&self) -> String {
        "relabel".to_owned()
    }
    pub fn build(
        &self,
        config: &SourceMiddlewareConfig,
    ) -> Result<RelabelMiddleware, MiddlewareSetupError> {
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

        Ok(RelabelMiddleware::new(relabel_config))
    }
}

#[cfg(feature = "engine-adapter")]
#[async_trait]
impl SourceMiddleware for RelabelMiddleware {
    async fn process(
        &self,
        change: SourceChange,
        _index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        self.apply(change).await
    }
}

#[cfg(feature = "engine-adapter")]
impl SourceMiddlewareFactory for RelabelMiddlewareFactory {
    fn name(&self) -> String {
        RelabelMiddlewareFactory::name(self)
    }
    fn create(
        &self,
        config: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        self.build(config)
            .map(|value| Arc::new(value) as Arc<dyn SourceMiddleware>)
    }
}
