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

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::in_memory_index::in_memory_element_index::InMemoryElementIndex;

    fn node(source: &str, id: &str, value: i64) -> Element {
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source, id),
                labels: Arc::from([Arc::from("Item")]),
                effective_from: 123,
            },
            properties: ElementPropertyMap::from(serde_json::json!({"value": value})),
        }
    }

    #[tokio::test]
    async fn values_and_elements_are_isolated_by_step_and_storage_kind() {
        let index = InMemoryElementIndex::new();
        let first_id = ComponentId::try_new("first").unwrap();
        let second_id = ComponentId::try_new("second").unwrap();
        let first = TransactionContext::new(
            &first_id,
            &index,
            StreamId::try_new("first/out").unwrap(),
            1,
        );
        let second = TransactionContext::new(
            &second_id,
            &index,
            StreamId::try_new("second/out").unwrap(),
            1,
        );
        for (context, value) in [(&first, 11), (&second, 22)] {
            assert_eq!(context.get("same-key").await.unwrap(), None);
            context
                .put("same-key", ElementValue::Integer(value))
                .await
                .unwrap();
            context
                .put_element(&node("values", "same-key", value + 1))
                .await
                .unwrap();
        }
        let reference = ElementReference::new("values", "same-key");
        assert_eq!(
            first.get("same-key").await.unwrap(),
            Some(ElementValue::Integer(11))
        );
        assert_eq!(
            second.get("same-key").await.unwrap(),
            Some(ElementValue::Integer(22))
        );
        assert_eq!(
            *first.get_element(&reference).await.unwrap().unwrap(),
            node("values", "same-key", 12)
        );
        assert_eq!(
            *second.get_element(&reference).await.unwrap().unwrap(),
            node("values", "same-key", 23)
        );
        first.remove("same-key").await.unwrap();
        first.remove_element(&reference).await.unwrap();
        first.remove("same-key").await.unwrap();
        first.remove_element(&reference).await.unwrap();
        assert_eq!(first.get("same-key").await.unwrap(), None);
        assert!(first.get_element(&reference).await.unwrap().is_none());
        assert_eq!(
            second.get("same-key").await.unwrap(),
            Some(ElementValue::Integer(22))
        );
        assert_eq!(
            *second.get_element(&reference).await.unwrap().unwrap(),
            node("values", "same-key", 23)
        );
    }

    #[tokio::test]
    async fn relation_endpoints_and_source_names_roundtrip_without_cross_step_aliases() {
        let index = InMemoryElementIndex::new();
        let step = ComponentId::try_new("relations").unwrap();
        let context = TransactionContext::new(
            &step,
            &index,
            StreamId::try_new("relations/out").unwrap(),
            1,
        );
        let relation = Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new("source/\u{00e9}", "relation"),
                labels: Arc::from([Arc::from("CONNECTS")]),
                effective_from: 7,
            },
            in_node: ElementReference::new("source/a", "same"),
            out_node: ElementReference::new("736f757263652f61", "same"),
            properties: ElementPropertyMap::from(serde_json::json!({"weight": 42})),
        };
        context.put_element(&relation).await.unwrap();
        assert_eq!(
            *context
                .get_element(relation.get_reference())
                .await
                .unwrap()
                .unwrap(),
            relation
        );
        for (source, value) in [("source/a", 1), ("736f757263652f61", 2), ("source/\u{00e9}", 3)] {
            let element = node(source, "same", value);
            context.put_element(&element).await.unwrap();
            assert_eq!(
                *context
                    .get_element(element.get_reference())
                    .await
                    .unwrap()
                    .unwrap(),
                element
            );
        }
        context
            .remove_element(&ElementReference::new("source/a", "same"))
            .await
            .unwrap();
        assert!(context
            .get_element(&ElementReference::new("736f757263652f61", "same"))
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            *context
                .get_element(relation.get_reference())
                .await
                .unwrap()
                .unwrap(),
            relation
        );
    }

    #[tokio::test]
    async fn empty_keys_and_unsupported_index_operations_fail_without_erasing_state() {
        let index = InMemoryElementIndex::new();
        let step = ComponentId::try_new("step").unwrap();
        let context =
            TransactionContext::new(&step, &index, StreamId::try_new("step/out").unwrap(), 1);
        context
            .put("keep", ElementValue::Integer(42))
            .await
            .unwrap();
        assert!(context.get("").await.is_err());
        assert!(context.put("", ElementValue::Null).await.is_err());
        assert!(context.remove("").await.is_err());
        assert!(matches!(
            context.middleware_index().clear().await,
            Err(IndexError::NotSupported)
        ));
        let reference = ElementReference::new("source", "node");
        assert!(matches!(
            context
                .middleware_index()
                .get_slot_elements_by_inbound(0, &reference)
                .await,
            Err(IndexError::NotSupported)
        ));
        assert!(matches!(
            context
                .middleware_index()
                .get_slot_elements_by_outbound(0, &reference)
                .await,
            Err(IndexError::NotSupported)
        ));
        assert_eq!(
            context.get("keep").await.unwrap(),
            Some(ElementValue::Integer(42))
        );
    }

    #[test]
    fn corrupt_or_foreign_element_namespaces_are_not_exposed_as_valid_step_state() {
        let index = InMemoryElementIndex::new();
        let step = ComponentId::try_new("step").unwrap();
        let context =
            TransactionContext::new(&step, &index, StreamId::try_new("step/out").unwrap(), 1);
        for suffix in ["z0", "0", "ff"] {
            let corrupt = node(
                &format!("{}elements/{suffix}", context.elements.prefix),
                "key",
                1,
            );
            assert!(context.elements.decode(&corrupt).is_err(), "{suffix}");
        }
        assert!(context
            .elements
            .decode(&node("another-step/elements/61", "key", 1))
            .is_err());
    }

    #[test]
    fn intermediate_identity_preserves_time_position_annotations_and_lineage() {
        use super::super::{ContextEntry, ContextValue, GraphChangeCodec};
        use drasi_core::models::SourceChange;

        let index = InMemoryElementIndex::new();
        let step = ComponentId::try_new("step").unwrap();
        let stream = StreamId::try_new("transaction/step/out").unwrap();
        let context = TransactionContext::new(&step, &index, stream.clone(), 41);
        let timestamp = chrono::DateTime::from_timestamp_millis(1_000).unwrap();
        let changes = GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: node("source", "one", 9),
            },
            StreamId::try_new("source/out").unwrap(),
            7,
            Some(timestamp),
        )
        .unwrap();
        let mut input = ChangeEnvelope::new(
            changes.id().clone(),
            changes.changes().clone(),
            changes
                .system()
                .as_ref()
                .clone()
                .with_source_position(bytes::Bytes::from_static(b"\0position")),
        );
        let annotation = ContextEntry::try_new(
            ComponentId::try_new("source").unwrap(),
            "trace",
            ContextValue::String("retained".into()),
        )
        .unwrap();
        input.append_annotation(annotation.clone()).unwrap();
        let output = context.derive(&input, input.changes().clone()).unwrap();
        assert_eq!(output.id(), &emission_id(&stream, 41).unwrap());
        assert_eq!(output.system().sequence(), 41);
        assert_eq!(output.system().timestamp(), Some(timestamp));
        assert_eq!(
            output.system().source_position(),
            input.system().source_position()
        );
        assert_eq!(
            output.context().entries().collect::<Vec<_>>(),
            vec![annotation]
        );
        assert_eq!(output.lineage().unwrap().envelope_id(), input.id());
        assert_eq!(output.changes().operations(), input.changes().operations());
        assert_ne!(output.changes().id(), input.changes().id());
        let finished = context.finish(output.clone()).unwrap();
        assert_eq!(finished.id(), output.id());
        assert_eq!(finished.lineage().unwrap().envelope_id(), input.id());
        assert_eq!(input.system().sequence(), 7);
    }
}
