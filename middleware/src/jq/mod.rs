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

use std::{cell::RefCell, collections::HashMap, sync::Arc};

use jq_rs::{self, JqProgram};

use async_trait::async_trait;
use drasi_core::{
    interface::{
        ElementIndex, MiddlewareError, MiddlewareSetupError, SourceMiddleware,
        SourceMiddlewareFactory,
    },
    models::{Element, ElementPropertyMap, ElementReference, SourceChange, SourceMiddlewareConfig},
};
use serde::Deserialize;
use serde_json::Value;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceMappedOutput {
    op: Option<MapOperation>,
    label: Option<String>,
    id: Option<String>,
    element_type: Option<MapElementType>,

    #[serde(default)]
    query: String,

    #[serde(default)]
    halt_on_error: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub enum MapOperation {
    Insert,
    Update,
    Delete,
}

#[derive(Debug, Clone, Deserialize)]
pub enum MapElementType {
    Node,
    #[serde(rename_all = "camelCase")]
    Relation {
        in_node_id: String,
        out_node_id: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceMappedOperations {
    #[serde(default)]
    insert: Vec<SourceMappedOutput>,

    #[serde(default)]
    update: Vec<SourceMappedOutput>,

    #[serde(default)]
    delete: Vec<SourceMappedOutput>,
}

thread_local! {
    static JQ_CACHE: RefCell<HashMap<String, RefCell<JqProgram>>> = RefCell::new(HashMap::new());
}

pub struct JQ {
    mappings: HashMap<String, SourceMappedOperations>,
}

impl JQ {
    fn new(mappings: HashMap<String, SourceMappedOperations>) -> Self {
        Self { mappings }
    }
}

#[async_trait]
impl SourceMiddleware for JQ {
    async fn process(
        &self,
        source_change: SourceChange,
        _element_index: &dyn ElementIndex,
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
                //todo: use Arc<str> instead of String
                Some(mappings) => match &source_change {
                    SourceChange::Insert { .. } => &mappings.insert,
                    SourceChange::Update { .. } => &mappings.update,
                    SourceChange::Delete { .. } => &mappings.delete,
                    _ => continue,
                },
                None => continue,
            };

            log::info!("Processing mappings for label: {}", label);

            #[allow(unused_assignments)]
            let mut del_element_binding = Option::<Element>::None;

            #[allow(clippy::unwrap_used)]
            let element = match &source_change {
                SourceChange::Insert { element } => element,
                SourceChange::Update { element } => element,
                SourceChange::Delete { metadata } => {
                    del_element_binding = Some(Element::Node {
                        metadata: metadata.clone(),
                        properties: ElementPropertyMap::new(),
                    });
                    del_element_binding.as_ref().unwrap()
                }
                _ => continue,
            };

            for mapping in mappings {
                let op = match &mapping.op {
                    Some(op) => op,
                    None => match &source_change {
                        SourceChange::Insert { .. } => &MapOperation::Insert,
                        SourceChange::Update { .. } => &MapOperation::Update,
                        SourceChange::Delete { .. } => &MapOperation::Delete,
                        _ => continue,
                    },
                };

                let source_obj: Value = element.into();

                let source_obj_str = match serde_json::to_string(&source_obj) {
                    Ok(s) => s,
                    Err(e) => {
                        log::error!("Failed to serialize source object to string: {e}");
                        if mapping.halt_on_error {
                            return Err(MiddlewareError::SourceChangeError(format!(
                                "Failed to serialize source object to string: {e}"
                            )));
                        }
                        continue;
                    }
                };

                let output = match run_jq(mapping.query.as_str(), &source_obj_str) {
                    Ok(output) => {
                        log::info!("JQ output: {}", output);
                        output
                    }
                    Err(e) => {
                        log::error!("{e}");
                        if mapping.halt_on_error {
                            return Err(e);
                        }
                        continue;
                    }
                };

                let query_output: Vec<Value> = if output.is_empty() {
                    continue;
                } else {
                    match serde_json::from_str::<Value>(&output) {
                        Ok(v) => {
                            if v.is_array() {
                                v.as_array().unwrap().to_vec()
                            } else {
                                vec![v]
                            }
                        }
                        Err(e) => {
                            log::error!("Failed to parse JQ output as JSON: {e}");
                            if mapping.halt_on_error {
                                return Err(MiddlewareError::SourceChangeError(format!(
                                    "Failed to parse JQ output as JSON: {e}"
                                )));
                            }
                            continue;
                        }
                    }
                };

                for item in query_output {
                    let mut new_metadata = metadata.clone();
                    let item_str = item.to_string();

                    if let Some(id) = &mapping.id {
                        new_metadata.reference.element_id =
                            match jq_get_string(id.as_str(), &item_str) {
                                Ok(id) => Arc::from(id),
                                Err(e) => {
                                    log::error!("Failed to get id from JQ expression: {e}");
                                    if mapping.halt_on_error {
                                        return Err(MiddlewareError::SourceChangeError(format!(
                                            "Failed to get id from JQ expression: {e}"
                                        )));
                                    }
                                    continue;
                                }
                            };
                    };

                    if let Some(label) = &mapping.label {
                        let new_label = match jq_get_string(label.as_str(), &item_str) {
                            Ok(l) => Arc::from(l),
                            Err(e) => {
                                log::error!("Failed to get label from JQ expression: {e}");
                                if mapping.halt_on_error {
                                    return Err(MiddlewareError::SourceChangeError(format!(
                                        "Failed to get label from JQ expression: {e}"
                                    )));
                                }
                                continue;
                            }
                        };
                        new_metadata.labels = Arc::from(vec![new_label]);
                    }

                    let new_element = match &mapping.element_type {
                        Some(MapElementType::Node) => Element::Node {
                            metadata: new_metadata,
                            properties: item.into(),
                        },
                        Some(MapElementType::Relation {
                            in_node_id,
                            out_node_id,
                        }) => {
                            let in_node_id = match jq_get_string(&in_node_id, &item_str) {
                                Ok(id) => Arc::from(id),
                                Err(e) => {
                                    log::error!("Failed to get in_node_id from JQ expression: {e}");
                                    if mapping.halt_on_error {
                                        return Err(MiddlewareError::SourceChangeError(format!(
                                            "Failed to get in_node_id from JQ expression: {e}"
                                        )));
                                    }
                                    continue;
                                }
                            };

                            let out_node_id = match jq_get_string(&out_node_id, &item_str) {
                                Ok(id) => Arc::from(id),
                                Err(e) => {
                                    log::error!(
                                        "Failed to get out_node_id from JQ expression: {e}"
                                    );
                                    if mapping.halt_on_error {
                                        return Err(MiddlewareError::SourceChangeError(format!(
                                            "Failed to get out_node_id from JQ expression: {e}"
                                        )));
                                    }
                                    continue;
                                }
                            };

                            Element::Relation {
                                metadata: new_metadata,
                                in_node: ElementReference::new(
                                    &metadata.reference.source_id,
                                    &in_node_id,
                                ),
                                out_node: ElementReference::new(
                                    &metadata.reference.source_id,
                                    &out_node_id,
                                ),
                                properties: item.into(),
                            }
                        }
                        None => match element {
                            Element::Node { .. } => Element::Node {
                                metadata: new_metadata,
                                properties: item.into(),
                            },
                            Element::Relation {
                                in_node, out_node, ..
                            } => Element::Relation {
                                metadata: new_metadata,
                                in_node: in_node.clone(),
                                out_node: out_node.clone(),
                                properties: item.into(),
                            },
                        },
                    };

                    match op {
                        MapOperation::Insert => results.push(SourceChange::Insert {
                            element: new_element,
                        }),
                        MapOperation::Update => results.push(SourceChange::Update {
                            element: new_element,
                        }),
                        MapOperation::Delete => results.push(SourceChange::Delete {
                            metadata: new_element.get_metadata().clone(),
                        }),
                    }
                }
            }
        }
        Ok(results)
    }
}

pub struct JQFactory {}

impl Default for JQFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl JQFactory {
    pub fn new() -> Self {
        JQFactory {}
    }
}

impl SourceMiddlewareFactory for JQFactory {
    fn name(&self) -> String {
        "jq".to_string()
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
                    "Invalid configuration: {e}"
                )))
            }
        };

        log::info!(
            "Map middleware {} mappings loaded: {:#?}",
            config.name,
            mappings
        );

        Ok(Arc::new(JQ::new(mappings)) as Arc<dyn SourceMiddleware>)
    }
}

fn run_jq(query: &str, input: &str) -> Result<String, MiddlewareError> {
    JQ_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();

        if !cache.contains_key(query) {
            let program = jq_rs::compile(query).map_err(|e| {
                MiddlewareError::SourceChangeError(format!("JQ compilation error: {e}"))
            })?;
            cache.insert(query.to_string(), RefCell::new(program));
        }

        let program_cell = cache.get(query).unwrap();
        let mut program = program_cell.borrow_mut();
        program
            .run(input)
            .map_err(|e| MiddlewareError::SourceChangeError(format!("JQ execution error: {e}")))
    })
}

fn jq_get_string(query: &str, input: &str) -> Result<String, MiddlewareError> {
    let output = run_jq(query, input)?;

    serde_json::from_str::<Value>(&output)
        .map_err(|e| {
            MiddlewareError::SourceChangeError(format!("Failed to parse JQ output as JSON: {e}"))
        })
        .and_then(|v| {
            if v.is_string() {
                Ok(v.as_str().unwrap().to_string())
            } else {
                Ok(v.to_string())
            }
        })
}
