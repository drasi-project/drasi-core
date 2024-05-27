use std::{
    collections::{BTreeMap, HashMap},
    ops::Deref,
    str::FromStr,
    sync::Arc,
};

use async_trait::async_trait;
use drasi_query_core::{
    interface::{MiddlewareError, MiddlewareSetupError, SourceMiddleware, SourceMiddlewareFactory},
    models::{Element, ElementPropertyMap, ElementReference, SourceChange, SourceMiddlewareConfig},
};
use jsonpath_rust::{path::config::JsonPathConfig, JsonPathInst};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceMappedOutput {
    op: Option<MapOperation>,
    selector: Option<JsonPathExpression>,
    label: Option<String>,
    id: Option<JsonPathExpression>,
    element_type: Option<MapElementType>,

    #[serde(default)]
    properties: BTreeMap<String, JsonPathExpression>,
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
        in_node_id: JsonPathExpression,
        out_node_id: JsonPathExpression,
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
        match result.first() {
            Some(v) => Some(v.deref().clone()),
            None => None,
        }
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

pub struct Map {
    mappings: HashMap<String, SourceMappedOperations>,
}

#[async_trait]
impl SourceMiddleware for Map {
    async fn process(
        &self,
        source_change: SourceChange,
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

                let mut source_obj: Value = element.into();

                let selected = match &mapping.selector {
                    Some(selector) => selector.execute(&source_obj),
                    None => vec![source_obj.clone()],
                };

                for s in selected {
                    log::info!("Processing mapping for selector: {:#?}", s);
                    if let Some(obj) = source_obj.as_object_mut() {
                        obj.insert("$selected".to_string(), s);
                    }

                    let mut new_metadata = metadata.clone();

                    if let Some(id) = &mapping.id {
                        new_metadata.reference.element_id = match id.execute_one(&source_obj) {
                            Some(id) => match id.as_str() {
                                Some(id) => Arc::from(id),
                                None => Arc::from(id.to_string().as_str()),
                            },
                            None => continue, //expression did not return an id
                        }
                    };

                    if let Some(label) = &mapping.label {
                        new_metadata.labels = Arc::from(vec![Arc::from(label.clone())]);
                    }

                    let mut properties = ElementPropertyMap::new();
                    for (key, value) in &mapping.properties {
                        match value.execute_one(&source_obj) {
                            Some(p) => properties.insert(key.as_str(), (&p).into()),
                            None => continue,
                        }
                    }

                    let new_element = match &mapping.element_type {
                        Some(MapElementType::Node) => Element::Node {
                            metadata: new_metadata,
                            properties,
                        },
                        Some(MapElementType::Relation {
                            in_node_id,
                            out_node_id,
                        }) => {
                            let in_node_id = match in_node_id.execute_one(&source_obj) {
                                Some(id) => match id.as_str() {
                                    Some(id) => Arc::from(id),
                                    None => Arc::from(id.to_string().as_str()),
                                },
                                None => continue, //expression did not return an id
                            };

                            let out_node_id = match out_node_id.execute_one(&source_obj) {
                                Some(id) => match id.as_str() {
                                    Some(id) => Arc::from(id),
                                    None => Arc::from(id.to_string().as_str()),
                                },
                                None => continue, //expression did not return an id
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
                                properties,
                            }
                        }
                        None => match element {
                            Element::Node { .. } => Element::Node {
                                metadata: new_metadata,
                                properties,
                            },
                            Element::Relation {
                                in_node, out_node, ..
                            } => Element::Relation {
                                metadata: new_metadata,
                                in_node: in_node.clone(),
                                out_node: out_node.clone(),
                                properties,
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

pub struct MapFactory {}

impl MapFactory {
    pub fn new() -> Self {
        MapFactory {}
    }
}

impl SourceMiddlewareFactory for MapFactory {
    fn name(&self) -> String {
        "map".to_string()
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
            "Map middleware {} mappings loaded: {:#?}",
            config.name,
            mappings
        );

        Ok(Arc::new(Map { mappings }))
    }
}
