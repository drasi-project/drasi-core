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
    cell::RefCell,
    collections::{HashMap, HashSet},
    sync::Arc,
};

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
use jq_rs::{self, JqProgram};
use serde::Deserialize;
use serde_json::{json, Value};

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

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub enum MapOperation {
    Insert,
    Update,
    Delete,
}

#[derive(Debug, Clone, Deserialize)]
pub enum MapElementType {
    Node,
    #[serde(alias = "relation")]
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtendedJqConfig {
    mappings: HashMap<String, SourceMappedOperations>,
    #[serde(default)]
    preserve_input: bool,
    #[serde(default)]
    include_source_metadata: bool,
    #[serde(default)]
    reconcile: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JqConfig {
    Extended(ExtendedJqConfig),
    Legacy(HashMap<String, SourceMappedOperations>),
}

thread_local! {
    static JQ_CACHE: RefCell<HashMap<String, RefCell<JqProgram>>> = RefCell::new(HashMap::new());
}

pub struct JQ {
    mappings: HashMap<String, SourceMappedOperations>,
    preserve_input: bool,
    include_source_metadata: bool,
    reconcile: bool,
}

impl JQ {
    fn new(config: JqConfig) -> Self {
        match config {
            JqConfig::Extended(config) => Self {
                mappings: config.mappings,
                preserve_input: config.preserve_input,
                include_source_metadata: config.include_source_metadata,
                reconcile: config.reconcile,
            },
            JqConfig::Legacy(mappings) => Self {
                mappings,
                preserve_input: false,
                include_source_metadata: false,
                reconcile: false,
            },
        }
    }

    async fn previous_element(
        &self,
        source_change: &SourceChange,
        element_index: &dyn ElementIndex,
    ) -> Result<Option<Arc<Element>>, MiddlewareError> {
        if !(self.reconcile
            || (self.include_source_metadata
                && matches!(source_change, SourceChange::Delete { .. })))
        {
            return Ok(None);
        }
        match source_change {
            SourceChange::Update { element } => {
                Ok(element_index.get_element(element.get_reference()).await?)
            }
            SourceChange::Delete { metadata } => {
                Ok(element_index.get_element(&metadata.reference).await?)
            }
            SourceChange::Insert { .. } | SourceChange::Future { .. } => Ok(None),
        }
    }

    fn mappings_for<'a>(
        &'a self,
        metadata: &ElementMetadata,
        operation: MapOperation,
    ) -> Vec<&'a SourceMappedOutput> {
        metadata
            .labels
            .iter()
            .flat_map(move |label| {
                self.mappings
                    .get(label.as_ref())
                    .map(|operations| match operation {
                        MapOperation::Insert => operations.insert.as_slice(),
                        MapOperation::Update => operations.update.as_slice(),
                        MapOperation::Delete => operations.delete.as_slice(),
                    })
                    .unwrap_or_default()
                    .iter()
            })
            .collect()
    }

    fn derive(
        &self,
        element: &Element,
        operation: MapOperation,
        delete_metadata: Option<&ElementMetadata>,
        previous_element: Option<&Element>,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        let metadata = delete_metadata.unwrap_or_else(|| element.get_metadata());
        let source_obj = self.source_object(
            element,
            operation,
            metadata,
            if operation == MapOperation::Delete {
                previous_element
            } else {
                Some(element)
            },
        );
        let source_obj_str = serde_json::to_string(&source_obj).map_err(|error| {
            MiddlewareError::SourceChangeError(format!(
                "Failed to serialize jq input object: {error}"
            ))
        })?;
        let mut results = Vec::new();

        for mapping in self.mappings_for(metadata, operation) {
            let output_operation = mapping.op.unwrap_or(operation);
            let output = match run_jq(&mapping.query, &source_obj_str) {
                Ok(output) => output,
                Err(error) if self.reconcile || mapping.halt_on_error => return Err(error),
                Err(error) => {
                    log::error!("{error}");
                    continue;
                }
            };

            let query_output = match parse_jq_output(&output) {
                Ok(output) => output,
                Err(error) if self.reconcile || mapping.halt_on_error => return Err(error),
                Err(error) => {
                    log::error!("{error}");
                    continue;
                }
            };

            for item in query_output {
                match self.map_item(&item, mapping, output_operation, metadata, element) {
                    Ok(change) => results.push(change),
                    Err(error) if self.reconcile || mapping.halt_on_error => return Err(error),
                    Err(error) => log::error!("{error}"),
                }
            }
        }

        Ok(results)
    }

    fn source_object(
        &self,
        element: &Element,
        operation: MapOperation,
        metadata: &ElementMetadata,
        endpoint_element: Option<&Element>,
    ) -> Value {
        let mut value: Value = element.into();
        if !self.include_source_metadata {
            return value;
        }

        let element_type = match endpoint_element {
            Some(Element::Relation { .. }) => "relation",
            Some(Element::Node { .. }) => "node",
            None => "unknown",
        };
        let mut source_metadata = json!({
            "operation": operation_name(operation),
            "sourceId": metadata.reference.source_id.as_ref(),
            "elementId": metadata.reference.element_id.as_ref(),
            "labels": metadata.labels.iter().map(|label| label.as_ref()).collect::<Vec<_>>(),
            "effectiveTime": metadata.effective_from,
            "elementType": element_type,
        });
        if let Some(Element::Relation {
            in_node, out_node, ..
        }) = endpoint_element
        {
            source_metadata["inNode"] = reference_value(in_node);
            source_metadata["outNode"] = reference_value(out_node);
        }
        if let Some(object) = value.as_object_mut() {
            object.insert("$source".to_string(), source_metadata);
        }
        value
    }

    fn map_item(
        &self,
        item: &Value,
        mapping: &SourceMappedOutput,
        operation: MapOperation,
        metadata: &ElementMetadata,
        source_element: &Element,
    ) -> Result<SourceChange, MiddlewareError> {
        let mut new_metadata = metadata.clone();
        let item_str = serde_json::to_string(item).map_err(|error| {
            MiddlewareError::SourceChangeError(format!(
                "Failed to serialize jq mapping output: {error}"
            ))
        })?;

        if let Some(id) = &mapping.id {
            new_metadata.reference.element_id = Arc::from(
                jq_get_string(id, &item_str).map_err(|error| contextual_error("id", error))?,
            );
        }
        if let Some(label) = &mapping.label {
            let label = jq_get_string(label, &item_str)
                .map_err(|error| contextual_error("label", error))?;
            new_metadata.labels = Arc::new([Arc::from(label)]);
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
                let in_node_id = jq_get_string(in_node_id, &item_str)
                    .map_err(|error| contextual_error("in_node_id", error))?;
                let out_node_id = jq_get_string(out_node_id, &item_str)
                    .map_err(|error| contextual_error("out_node_id", error))?;
                Element::Relation {
                    metadata: new_metadata,
                    in_node: ElementReference::new(&metadata.reference.source_id, &in_node_id),
                    out_node: ElementReference::new(&metadata.reference.source_id, &out_node_id),
                    properties: item.into(),
                }
            }
            None => match source_element {
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

        Ok(match operation {
            MapOperation::Insert => SourceChange::Insert {
                element: new_element,
            },
            MapOperation::Update => SourceChange::Update {
                element: new_element,
            },
            MapOperation::Delete => SourceChange::Delete {
                metadata: new_element.get_metadata().clone(),
            },
        })
    }

    fn live_outputs(
        &self,
        element: &Element,
    ) -> Result<HashMap<ElementReference, ElementMetadata>, MiddlewareError> {
        let mut outputs = HashMap::new();
        for operation in [MapOperation::Insert, MapOperation::Update] {
            for change in self.derive(element, operation, None, Some(element))? {
                match change {
                    SourceChange::Insert { element } | SourceChange::Update { element } => {
                        outputs.insert(
                            element.get_reference().clone(),
                            element.get_metadata().clone(),
                        );
                    }
                    SourceChange::Delete { .. } | SourceChange::Future { .. } => {}
                }
            }
        }
        Ok(outputs)
    }

    async fn reconciliation_deletes(
        &self,
        source_change: &SourceChange,
        previous_element: Option<&Element>,
        current_changes: &[SourceChange],
        element_index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        if !self.reconcile || matches!(source_change, SourceChange::Insert { .. }) {
            return Ok(Vec::new());
        }
        let Some(previous_element) = previous_element else {
            log::warn!(
                "jq reconciliation could not find previous source element {}",
                source_change.get_reference()
            );
            return Ok(Vec::new());
        };

        let current_references: HashSet<&ElementReference> = current_changes
            .iter()
            .map(|change| match change {
                SourceChange::Insert { element } | SourceChange::Update { element } => {
                    element.get_reference()
                }
                SourceChange::Delete { metadata } => &metadata.reference,
                SourceChange::Future { future_ref } => &future_ref.element_ref,
            })
            .collect();
        let effective_from = source_change.get_transaction_time();
        let mut deletes = Vec::new();
        for (reference, mut metadata) in self.live_outputs(previous_element)? {
            if current_references.contains(&reference) {
                continue;
            }
            // Insert and update mappings may differ. Only an indexed candidate
            // was actually materialized by an earlier source change.
            if element_index.get_element(&reference).await?.is_some() {
                metadata.effective_from = effective_from;
                deletes.push(SourceChange::Delete { metadata });
            }
        }
        Ok(deletes)
    }
}

#[async_trait]
impl SourceMiddleware for JQ {
    async fn process(
        &self,
        source_change: SourceChange,
        element_index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        if matches!(source_change, SourceChange::Future { .. }) {
            return Ok(vec![source_change]);
        }

        let previous_element = self.previous_element(&source_change, element_index).await?;
        if self.include_source_metadata
            && matches!(source_change, SourceChange::Delete { .. })
            && previous_element.is_none()
        {
            log::warn!(
                "jq delete metadata for {} has no indexed source element; elementType is unknown and relation endpoints are unavailable",
                source_change.get_reference()
            );
        }
        let operation = source_change_operation(&source_change);
        let delete_element;
        let (element, delete_metadata) = match &source_change {
            SourceChange::Insert { element } | SourceChange::Update { element } => (element, None),
            SourceChange::Delete { metadata } => {
                delete_element = Element::Node {
                    metadata: metadata.clone(),
                    properties: ElementPropertyMap::new(),
                };
                (&delete_element, Some(metadata))
            }
            SourceChange::Future { .. } => unreachable!(),
        };

        let mut derived = self.derive(
            element,
            operation,
            delete_metadata,
            previous_element.as_deref(),
        )?;
        let mut results = self
            .reconciliation_deletes(
                &source_change,
                previous_element.as_deref(),
                &derived,
                element_index,
            )
            .await?;
        results.append(&mut derived);
        if self.preserve_input {
            results.push(source_change);
        }
        Ok(results)
    }
}

fn source_change_operation(change: &SourceChange) -> MapOperation {
    match change {
        SourceChange::Insert { .. } => MapOperation::Insert,
        SourceChange::Update { .. } => MapOperation::Update,
        SourceChange::Delete { .. } => MapOperation::Delete,
        SourceChange::Future { .. } => unreachable!(),
    }
}

fn operation_name(operation: MapOperation) -> &'static str {
    match operation {
        MapOperation::Insert => "insert",
        MapOperation::Update => "update",
        MapOperation::Delete => "delete",
    }
}

fn reference_value(reference: &ElementReference) -> Value {
    json!({
        "sourceId": reference.source_id.as_ref(),
        "elementId": reference.element_id.as_ref(),
    })
}

fn parse_jq_output(output: &str) -> Result<Vec<Value>, MiddlewareError> {
    if output.is_empty() {
        return Ok(Vec::new());
    }
    let value = serde_json::from_str::<Value>(output).map_err(|error| {
        MiddlewareError::SourceChangeError(format!("Failed to parse jq output as JSON: {error}"))
    })?;
    match value {
        Value::Array(values) => Ok(values),
        value => Ok(vec![value]),
    }
}

fn contextual_error(context: &str, error: MiddlewareError) -> MiddlewareError {
    MiddlewareError::SourceChangeError(format!("Failed to resolve jq {context}: {error}"))
}

pub struct JQFactory;

impl Default for JQFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl JQFactory {
    pub fn new() -> Self {
        Self
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
        let uses_extended_options = ["preserve_input", "include_source_metadata", "reconcile"]
            .iter()
            .any(|key| config.config.get(*key).is_some_and(Value::is_boolean));
        if uses_extended_options && !config.config.contains_key("mappings") {
            return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                "[{}] Extended jq configuration must place label mappings under 'mappings'",
                config.name
            )));
        }
        let parsed_config: JqConfig = serde_json::from_value(Value::Object(config.config.clone()))
            .map_err(|error| {
                MiddlewareSetupError::InvalidConfiguration(format!(
                    "[{}] Invalid jq configuration: {error}",
                    config.name
                ))
            })?;
        if let JqConfig::Extended(extended) = &parsed_config {
            if extended.reconcile && !extended.preserve_input {
                return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                    "[{}] jq 'reconcile' requires 'preserve_input: true'",
                    config.name
                )));
            }
        }

        Ok(Arc::new(JQ::new(parsed_config)))
    }
}

fn run_jq(query: &str, input: &str) -> Result<String, MiddlewareError> {
    JQ_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();

        if !cache.contains_key(query) {
            let program = jq_rs::compile(query).map_err(|error| {
                MiddlewareError::SourceChangeError(format!("JQ compilation error: {error}"))
            })?;
            cache.insert(query.to_string(), RefCell::new(program));
        }

        let program_cell = cache.get(query).ok_or_else(|| {
            MiddlewareError::SourceChangeError("JQ program not found in cache".to_string())
        })?;
        let result = program_cell.borrow_mut().run(input).map_err(|error| {
            MiddlewareError::SourceChangeError(format!("JQ execution error: {error}"))
        });
        result
    })
}

#[allow(clippy::bind_instead_of_map)]
fn jq_get_string(query: &str, input: &str) -> Result<String, MiddlewareError> {
    let output = run_jq(query, input)?;

    serde_json::from_str::<Value>(&output)
        .map_err(|error| {
            MiddlewareError::SourceChangeError(format!(
                "Failed to parse JQ output as JSON: {error}"
            ))
        })
        .and_then(|value| {
            if let Some(value) = value.as_str() {
                Ok(value.to_string())
            } else {
                Ok(value.to_string())
            }
        })
}
