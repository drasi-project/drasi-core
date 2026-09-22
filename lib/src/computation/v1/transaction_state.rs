// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use async_trait::async_trait;
use drasi_core::{
    interface::{ElementIndex, ElementStream, IndexError},
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, QueryJoin,
    },
    path_solver::match_path::MatchPath,
};

use super::{
    emission_id, ChangeEnvelope, ChangeSet, ChangeSetId, ChangeSetRef, ComponentId, StreamId,
    SystemMetadata,
};

fn encode(value: &str) -> String {
    value.bytes().map(|byte| format!("{byte:02x}")).collect()
}

/// Storage borrowed for one step of one active transaction. It cannot be cloned,
/// moved to a background task, used to access another step, or used to commit.
///
/// Store commit-sensitive state here, not in mutable fields on the transformer.
/// Each operation joins the container's transaction, including read-your-writes.
pub struct TransactionContext<'a> {
    step: &'a ComponentId,
    elements: StepElements<'a>,
    stream: StreamId,
    sequence: u64,
}

impl<'a> TransactionContext<'a> {
    pub(super) fn new(
        step: &'a ComponentId,
        index: &'a dyn ElementIndex,
        stream: StreamId,
        sequence: u64,
    ) -> Self {
        Self {
            step,
            elements: StepElements {
                prefix: format!("transaction-step/{}/", encode(step.as_str())),
                index,
                unsupported: AtomicBool::new(false),
            },
            stream,
            sequence,
        }
    }

    pub fn step_id(&self) -> &ComponentId {
        self.step
    }

    /// Assign an intermediate event identity owned by this step and transaction,
    /// retaining the input's annotations, time, position and lineage.
    pub fn derive(
        &self,
        input: &ChangeEnvelope,
        changes: ChangeSetRef,
    ) -> anyhow::Result<ChangeEnvelope> {
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new(
                self.stream.as_str(),
                bytes::Bytes::copy_from_slice(&self.sequence.to_be_bytes()),
            )?,
            changes.schema().clone(),
            changes.operations().to_vec(),
        )?;
        let mut system = SystemMetadata::new(self.stream.clone(), self.sequence);
        if let Some(timestamp) = input.system().timestamp() {
            system = system.with_timestamp(timestamp);
        }
        if let Some(position) = input.system().source_position() {
            system = system.with_source_position(position.clone());
        }
        Ok(input.derive(emission_id(&self.stream, self.sequence)?, changes, system))
    }

    pub(super) fn finish(&self, output: ChangeEnvelope) -> anyhow::Result<ChangeEnvelope> {
        if output.id() == &emission_id(&self.stream, self.sequence)?
            && output.system().stream() == &self.stream
            && output.system().sequence() == self.sequence
        {
            Ok(output)
        } else {
            self.derive(&output, output.changes().clone())
        }
    }

    pub async fn get(&self, key: &str) -> anyhow::Result<Option<ElementValue>> {
        let reference = self.value_reference(key)?;
        let Some(element) = self.elements.index.get_element(&reference).await? else {
            return Ok(None);
        };
        match element.as_ref() {
            Element::Node { properties, .. } => properties
                .get("value")
                .cloned()
                .map(Some)
                .ok_or_else(|| anyhow::anyhow!("transaction step value is corrupt")),
            _ => anyhow::bail!("transaction step value is not a node"),
        }
    }

    pub async fn put(&self, key: &str, value: ElementValue) -> anyhow::Result<()> {
        let mut properties = ElementPropertyMap::new();
        properties.insert("value", value);
        self.elements
            .index
            .set_element(
                &Element::Node {
                    metadata: ElementMetadata {
                        reference: self.value_reference(key)?,
                        labels: Arc::from([]),
                        effective_from: 0,
                    },
                    properties,
                },
                &Vec::new(),
            )
            .await?;
        Ok(())
    }

    pub async fn remove(&self, key: &str) -> anyhow::Result<()> {
        self.elements
            .index
            .delete_element(&self.value_reference(key)?)
            .await?;
        Ok(())
    }

    fn value_reference(&self, key: &str) -> anyhow::Result<ElementReference> {
        anyhow::ensure!(!key.is_empty(), "transaction state keys must not be empty");
        Ok(ElementReference::new(
            &format!("{}values", self.elements.prefix),
            key,
        ))
    }

    pub async fn get_element(
        &self,
        reference: &ElementReference,
    ) -> anyhow::Result<Option<Arc<Element>>> {
        Ok(self.elements.get_element(reference).await?)
    }

    pub async fn put_element(&self, element: &Element) -> anyhow::Result<()> {
        self.elements.set_element(element, &Vec::new()).await?;
        Ok(())
    }

    pub async fn remove_element(&self, reference: &ElementReference) -> anyhow::Result<()> {
        self.elements.delete_element(reference).await?;
        Ok(())
    }

    pub(super) fn middleware_index(&self) -> &dyn ElementIndex {
        &self.elements
    }

    pub(super) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.elements.unsupported.load(Ordering::Acquire),
            "middleware attempted unsupported index reconfiguration inside a transaction"
        );
        Ok(())
    }
}

struct StepElements<'a> {
    prefix: String,
    index: &'a dyn ElementIndex,
    unsupported: AtomicBool,
}

impl StepElements<'_> {
    fn reference(&self, reference: &ElementReference) -> ElementReference {
        ElementReference::new(
            &format!("{}elements/{}", self.prefix, encode(&reference.source_id)),
            &reference.element_id,
        )
    }

    fn encode(&self, element: &Element) -> Element {
        let mut element = element.clone();
        match &mut element {
            Element::Node { metadata, .. } => {
                metadata.reference = self.reference(&metadata.reference);
            }
            Element::Relation {
                metadata,
                in_node,
                out_node,
                ..
            } => {
                metadata.reference = self.reference(&metadata.reference);
                *in_node = self.reference(in_node);
                *out_node = self.reference(out_node);
            }
        }
        element
    }

    fn decode_reference(&self, reference: &mut ElementReference) -> Result<(), IndexError> {
        let encoded = reference
            .source_id
            .strip_prefix(&format!("{}elements/", self.prefix))
            .ok_or(IndexError::CorruptedData)?;
        if !encoded.is_ascii() || encoded.len() % 2 != 0 {
            return Err(IndexError::CorruptedData);
        }
        let bytes = (0..encoded.len())
            .step_by(2)
            .map(|offset| u8::from_str_radix(&encoded[offset..offset + 2], 16))
            .collect::<Result<Vec<_>, _>>()
            .map_err(IndexError::other)?;
        reference.source_id = String::from_utf8(bytes).map_err(IndexError::other)?.into();
        Ok(())
    }

    fn decode(&self, element: &Element) -> Result<Element, IndexError> {
        let mut element = element.clone();
        match &mut element {
            Element::Node { metadata, .. } => self.decode_reference(&mut metadata.reference)?,
            Element::Relation {
                metadata,
                in_node,
                out_node,
                ..
            } => {
                self.decode_reference(&mut metadata.reference)?;
                self.decode_reference(in_node)?;
                self.decode_reference(out_node)?;
            }
        }
        Ok(element)
    }
}

#[async_trait]
impl ElementIndex for StepElements<'_> {
    async fn get_element(
        &self,
        reference: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        self.index
            .get_element(&self.reference(reference))
            .await?
            .map(|element| self.decode(&element).map(Arc::new))
            .transpose()
    }

    async fn set_element(&self, element: &Element, slots: &Vec<usize>) -> Result<(), IndexError> {
        self.index.set_element(&self.encode(element), slots).await
    }

    async fn delete_element(&self, reference: &ElementReference) -> Result<(), IndexError> {
        self.index.delete_element(&self.reference(reference)).await
    }

    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        self.index
            .get_slot_element_by_ref(slot, &self.reference(reference))
            .await?
            .map(|element| self.decode(&element).map(Arc::new))
            .transpose()
    }

    async fn get_slot_elements_by_inbound(
        &self,
        _: usize,
        _: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        Err(IndexError::NotSupported)
    }

    async fn get_slot_elements_by_outbound(
        &self,
        _: usize,
        _: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        Err(IndexError::NotSupported)
    }

    async fn clear(&self) -> Result<(), IndexError> {
        Err(IndexError::NotSupported)
    }

    async fn set_joins(&self, _: &MatchPath, _: &Vec<Arc<QueryJoin>>) {
        self.unsupported.store(true, Ordering::Release);
        log::error!("Transaction middleware cannot reconfigure shared index joins");
    }
}
