// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A stateful transformer's own durable state, kept in its host element index
//! in the same processing operation as its output and input progress. A failed
//! operation discards it with everything else the operation staged, and a
//! rebuild starts it empty, so it never disagrees with the committed output.
//!
//! - **Input images.** The complete current image of every input element, with
//!   additions, replacements, patches and deletions applied in order
//!   ([`apply_inputs`]).
//! - **Private tables.** Named key-value tables for state derived from those
//!   inputs, such as which outputs each input produced ([`get_value`],
//!   [`set_value`]). [`get_values`] reads a whole table, for example to
//!   rebuild in-memory structures when the state opens.

use std::{
    collections::{hash_map::Entry, HashMap},
    sync::Arc,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{ElementImage, ElementPatch};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use serde::{de::DeserializeOwned, Serialize};

use super::binding::ContextBinding;
use crate::NativeProcessingContext;

/// The durable state a transformer's logic reads and writes in one operation.
#[async_trait]
pub trait StateStore: Send + Sync {
    /// The retained image of one input.
    async fn input(&self, reference: &ElementReference) -> Result<Option<Element>>;
    /// Retain an input's complete image.
    async fn set_input(&self, element: &Element) -> Result<()>;
    /// Forget an input.
    async fn delete_input(&self, reference: &ElementReference) -> Result<()>;
    /// Every retained input image.
    async fn inputs(&self) -> Result<Vec<Element>>;
    /// One serialized value from a private table.
    async fn value(&self, table: &str, key: &str) -> Result<Option<String>>;
    /// Store one serialized value in a private table.
    async fn set_serialized(&self, table: &str, key: &str, value: String) -> Result<()>;
    /// Remove one value from a private table.
    async fn delete_value(&self, table: &str, key: &str) -> Result<()>;
    /// Every entry of one private table, as `(key, serialized value)`.
    async fn values(&self, table: &str) -> Result<Vec<(String, String)>>;
}

/// One input whose retained image a batch of changes affected.
#[derive(Debug, Clone, PartialEq)]
pub struct InputChange {
    pub reference: ElementReference,
    /// The retained image before the batch, if there was one.
    pub previous: Option<Element>,
    /// The retained image after the batch, or `None` once it is deleted.
    pub current: Option<Element>,
    /// The effective time of the batch's last change to this input.
    pub effective_from: u64,
}

/// Apply a batch of changes to the retained input images, in order. Returns
/// each affected input once, in the order the batch first touched it.
///
/// Additions and replacements retain the complete image. A patch applies to
/// the retained image and fails without one. Deleting an absent input is a
/// no-op. Scheduled notifications are not input images and are rejected.
pub async fn apply_inputs<S: StateStore + ?Sized>(
    store: &S,
    changes: &[SourceChange],
) -> Result<Vec<InputChange>> {
    apply_inputs_where(store, changes, |_| true).await
}

/// [`apply_inputs`] for a transformer that retains only the inputs it reads.
/// An image `retain` rejects is forgotten as if it were deleted, and a patch
/// for an input that is not retained fails only when `retain` accepts the
/// patch's own image; otherwise it is ignored.
pub async fn apply_inputs_where<S, F>(
    store: &S,
    changes: &[SourceChange],
    retain: F,
) -> Result<Vec<InputChange>>
where
    S: StateStore + ?Sized,
    F: Fn(&Element) -> bool + Sync,
{
    let mut order = Vec::new();
    let mut working: HashMap<ElementReference, InputChange> = HashMap::new();
    for change in changes {
        change.validate()?;
        let metadata = match change {
            SourceChange::Insert { element }
            | SourceChange::Replace { element }
            | SourceChange::Update { element, .. } => element.get_metadata(),
            SourceChange::Delete { metadata } => metadata,
            SourceChange::Future { .. } => {
                anyhow::bail!("a scheduled notification is not an input image")
            }
        };
        let reference = metadata.reference.clone();
        let input = match working.entry(reference.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                order.push(reference.clone());
                let retained = store.input(&reference).await?;
                entry.insert(InputChange {
                    reference: reference.clone(),
                    previous: retained.clone(),
                    current: retained,
                    effective_from: metadata.effective_from,
                })
            }
        };
        input.effective_from = metadata.effective_from;
        input.current = match change {
            SourceChange::Insert { element } | SourceChange::Replace { element } => {
                retain(element).then(|| element.clone())
            }
            SourceChange::Update {
                element,
                removed_properties,
            } => match &input.current {
                Some(base) => {
                    let patch = ElementPatch::try_new(
                        ElementImage::try_from(element.clone())?,
                        removed_properties.clone(),
                    )?;
                    Some(patch.apply_to(Some(base))?).filter(|image| retain(image))
                }
                None if retain(element) => {
                    anyhow::bail!("patch for {reference} has no retained full input image")
                }
                None => None,
            },
            SourceChange::Delete { .. } => None,
            SourceChange::Future { .. } => unreachable!("rejected above"),
        };
    }
    let mut affected = Vec::with_capacity(order.len());
    for reference in order {
        let input = working
            .remove(&reference)
            .context("affected input was not tracked")?;
        match (&input.previous, &input.current) {
            (_, Some(element)) => store.set_input(element).await?,
            (Some(_), None) => store.delete_input(&reference).await?,
            (None, None) => {}
        }
        affected.push(input);
    }
    Ok(affected)
}

/// A value from one of the transformer's private tables.
pub async fn get_value<V: DeserializeOwned, S: StateStore + ?Sized>(
    store: &S,
    table: &str,
    key: &str,
) -> Result<Option<V>> {
    store
        .value(table, key)
        .await?
        .map(|value| {
            serde_json::from_str(&value)
                .with_context(|| format!("private table entry {table}/{key} is malformed"))
        })
        .transpose()
}

/// Every entry of one of the transformer's private tables, ordered by key.
pub async fn get_values<V: DeserializeOwned, S: StateStore + ?Sized>(
    store: &S,
    table: &str,
) -> Result<Vec<(String, V)>> {
    let mut values = store
        .values(table)
        .await?
        .into_iter()
        .map(|(key, value)| {
            let value = serde_json::from_str(&value)
                .with_context(|| format!("private table entry {table}/{key} is malformed"))?;
            Ok((key, value))
        })
        .collect::<Result<Vec<_>>>()?;
    values.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(values)
}

/// Store a value in one of the transformer's private tables.
pub async fn set_value<V: Serialize, S: StateStore + ?Sized>(
    store: &S,
    table: &str,
    key: &str,
    value: &V,
) -> Result<()> {
    store
        .set_serialized(table, key, serde_json::to_string(value)?)
        .await
}

/// The element-index slot holding the current input images.
const INPUT_SLOT: usize = 0;
/// The element-index slot holding private table entries, so a table can be
/// read whole without its entries appearing among the inputs.
const TABLE_SLOT: usize = 1;
/// Private table entries use this namespace prefix, which input elements never
/// carry.
const TABLE_NAMESPACE: &str = "\u{0}drasi.transformer/";
const VALUE_PROPERTY: &str = "value";

fn table_namespace(table: &str) -> String {
    format!("{TABLE_NAMESPACE}{table}")
}

fn table_reference(table: &str, key: &str) -> ElementReference {
    ElementReference::new(&table_namespace(table), key)
}

fn table_value(table: &str, element: &Element) -> Result<String> {
    match element {
        Element::Node { properties, .. } => match properties.get(VALUE_PROPERTY) {
            Some(ElementValue::String(value)) => Ok(value.to_string()),
            _ => anyhow::bail!(
                "private table entry {table}/{} is malformed",
                element.get_reference().element_id
            ),
        },
        Element::Relation { .. } => anyhow::bail!(
            "private table entry {table}/{} is malformed",
            element.get_reference().element_id
        ),
    }
}

/// The state kept in the host element index of the bound operation.
#[derive(Default)]
pub struct HostState(ContextBinding<NativeProcessingContext>);

impl HostState {
    /// Bind this operation's context. Keep the guard until the operation ends.
    pub fn bind(&self, context: &Arc<NativeProcessingContext>) -> Result<Box<dyn Send>> {
        Ok(Box::new(self.0.bind(context.clone())?))
    }

    fn context(&self) -> Result<Arc<NativeProcessingContext>> {
        Ok(self.0.get()?)
    }
}

#[async_trait]
impl StateStore for HostState {
    async fn input(&self, reference: &ElementReference) -> Result<Option<Element>> {
        Ok(self.context()?.get_element(reference).await?)
    }

    async fn set_input(&self, element: &Element) -> Result<()> {
        Ok(self.context()?.set_element(element, &[INPUT_SLOT]).await?)
    }

    async fn delete_input(&self, reference: &ElementReference) -> Result<()> {
        Ok(self.context()?.delete_element(reference).await?)
    }

    async fn inputs(&self) -> Result<Vec<Element>> {
        let context = self.context()?;
        let stream = context.open_slot(INPUT_SLOT).await?;
        let mut inputs = Vec::new();
        while let Some(element) = context.next_slot(stream).await? {
            inputs.push(element);
        }
        Ok(inputs)
    }

    async fn value(&self, table: &str, key: &str) -> Result<Option<String>> {
        self.context()?
            .get_element(&table_reference(table, key))
            .await?
            .map(|element| table_value(table, &element))
            .transpose()
    }

    async fn set_serialized(&self, table: &str, key: &str, value: String) -> Result<()> {
        let mut properties = ElementPropertyMap::new();
        properties.insert(VALUE_PROPERTY, ElementValue::String(value.into()));
        let entry = Element::Node {
            metadata: ElementMetadata {
                reference: table_reference(table, key),
                labels: Arc::from(Vec::<Arc<str>>::new()),
                effective_from: 0,
            },
            properties,
        };
        Ok(self.context()?.set_element(&entry, &[TABLE_SLOT]).await?)
    }

    async fn delete_value(&self, table: &str, key: &str) -> Result<()> {
        Ok(self
            .context()?
            .delete_element(&table_reference(table, key))
            .await?)
    }

    async fn values(&self, table: &str) -> Result<Vec<(String, String)>> {
        let context = self.context()?;
        let namespace = table_namespace(table);
        let stream = context.open_slot(TABLE_SLOT).await?;
        let mut values = Vec::new();
        while let Some(element) = context.next_slot(stream).await? {
            let reference = element.get_reference();
            if *reference.namespace == *namespace {
                values.push((
                    reference.element_id.to_string(),
                    table_value(table, &element)?,
                ));
            }
        }
        Ok(values)
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use memory::{MemoryState, MemoryTransaction};

#[cfg(any(test, feature = "test-support"))]
mod memory {
    use std::{collections::HashMap, sync::Mutex};

    use anyhow::Result;
    use async_trait::async_trait;
    use drasi_core_types::models::{Element, ElementReference};

    use super::StateStore;

    #[derive(Default, Clone)]
    struct Data {
        inputs: HashMap<ElementReference, Element>,
        tables: HashMap<(String, String), String>,
    }

    /// In-memory state for testing transformer logic without a host. Changes
    /// made through a [`MemoryTransaction`] apply only when it commits, like a
    /// host processing operation.
    #[derive(Default)]
    pub struct MemoryState(Mutex<Data>);

    impl MemoryState {
        /// Begin an operation over a copy of the committed state.
        pub fn transaction(&self) -> MemoryTransaction<'_> {
            let data = self.0.lock().expect("memory state poisoned").clone();
            MemoryTransaction {
                committed: self,
                staged: Mutex::new(data),
            }
        }

        /// The committed value of one private table entry.
        pub fn committed_value(&self, table: &str, key: &str) -> Option<String> {
            self.0
                .lock()
                .expect("memory state poisoned")
                .tables
                .get(&(table.to_owned(), key.to_owned()))
                .cloned()
        }

        /// Every committed private table entry, as `(table, key, value)`.
        pub fn committed_values(&self) -> Vec<(String, String, String)> {
            self.0
                .lock()
                .expect("memory state poisoned")
                .tables
                .iter()
                .map(|((table, key), value)| (table.clone(), key.clone(), value.clone()))
                .collect()
        }

        /// The committed image of one input.
        pub fn committed_input(&self, reference: &ElementReference) -> Option<Element> {
            self.0
                .lock()
                .expect("memory state poisoned")
                .inputs
                .get(reference)
                .cloned()
        }
    }

    /// One operation's staged changes to a [`MemoryState`]. Dropping it
    /// without [`commit`](Self::commit) discards them.
    pub struct MemoryTransaction<'a> {
        committed: &'a MemoryState,
        staged: Mutex<Data>,
    }

    impl MemoryTransaction<'_> {
        /// Make the staged changes the committed state.
        pub fn commit(self) {
            let data = self.staged.into_inner().expect("memory state poisoned");
            *self.committed.0.lock().expect("memory state poisoned") = data;
        }

        fn with<T>(&self, f: impl FnOnce(&mut Data) -> T) -> T {
            f(&mut self.staged.lock().expect("memory state poisoned"))
        }
    }

    #[async_trait]
    impl StateStore for MemoryTransaction<'_> {
        async fn input(&self, reference: &ElementReference) -> Result<Option<Element>> {
            Ok(self.with(|data| data.inputs.get(reference).cloned()))
        }

        async fn set_input(&self, element: &Element) -> Result<()> {
            self.with(|data| {
                data.inputs
                    .insert(element.get_reference().clone(), element.clone())
            });
            Ok(())
        }

        async fn delete_input(&self, reference: &ElementReference) -> Result<()> {
            self.with(|data| data.inputs.remove(reference));
            Ok(())
        }

        async fn inputs(&self) -> Result<Vec<Element>> {
            Ok(self.with(|data| data.inputs.values().cloned().collect()))
        }

        async fn value(&self, table: &str, key: &str) -> Result<Option<String>> {
            Ok(self.with(|data| {
                data.tables
                    .get(&(table.to_owned(), key.to_owned()))
                    .cloned()
            }))
        }

        async fn set_serialized(&self, table: &str, key: &str, value: String) -> Result<()> {
            self.with(|data| {
                data.tables
                    .insert((table.to_owned(), key.to_owned()), value)
            });
            Ok(())
        }

        async fn delete_value(&self, table: &str, key: &str) -> Result<()> {
            self.with(|data| data.tables.remove(&(table.to_owned(), key.to_owned())));
            Ok(())
        }

        async fn values(&self, table: &str) -> Result<Vec<(String, String)>> {
            Ok(self.with(|data| {
                data.tables
                    .iter()
                    .filter(|((name, _), _)| name == table)
                    .map(|((_, key), value)| (key.clone(), value.clone()))
                    .collect()
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core_types::{
        interface::FutureElementRef,
        models::{ElementMetadata, ElementPropertyMap},
    };

    fn node(id: &str, time: u64, properties: serde_json::Value) -> Element {
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", id),
                labels: vec![Arc::from("Input")].into(),
                effective_from: time,
            },
            properties: ElementPropertyMap::from(properties),
        }
    }

    fn deletion(id: &str, time: u64) -> SourceChange {
        SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", id),
                labels: Arc::from([]),
                effective_from: time,
            },
        }
    }

    #[tokio::test]
    async fn a_batch_reports_each_input_once_with_its_previous_and_final_image() -> Result<()> {
        let state = MemoryState::default();
        let first = state.transaction();
        apply_inputs(
            &first,
            &[SourceChange::Insert {
                element: node("kept", 1, serde_json::json!({"a": 1, "b": 2})),
            }],
        )
        .await?;
        first.commit();

        let batch = state.transaction();
        let affected = apply_inputs(
            &batch,
            &[
                SourceChange::Update {
                    element: node("kept", 2, serde_json::json!({"c": 3})),
                    removed_properties: ["b".into()].into(),
                },
                SourceChange::Insert {
                    element: node("brief", 2, serde_json::json!({})),
                },
                deletion("brief", 3),
                deletion("absent", 3),
                SourceChange::update(node("kept", 4, serde_json::json!({"a": null}))),
            ],
        )
        .await?;
        assert_eq!(
            affected
                .iter()
                .map(|input| input.reference.element_id.as_ref())
                .collect::<Vec<_>>(),
            ["kept", "brief", "absent"]
        );
        assert_eq!(
            affected[0].previous,
            Some(node("kept", 1, serde_json::json!({"a": 1, "b": 2})))
        );
        assert_eq!(
            affected[0].current,
            Some(node("kept", 4, serde_json::json!({"a": null, "c": 3})))
        );
        assert_eq!(affected[0].effective_from, 4);
        assert_eq!(
            (&affected[1].previous, &affected[1].current),
            (&None, &None)
        );
        assert_eq!(
            (&affected[2].previous, &affected[2].current),
            (&None, &None)
        );
        assert_eq!(
            batch.inputs().await?,
            vec![node("kept", 4, serde_json::json!({"a": null, "c": 3}))]
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_patch_needs_a_retained_image_and_a_failed_batch_changes_nothing() -> Result<()> {
        let state = MemoryState::default();
        let batch = state.transaction();
        let error = apply_inputs(
            &batch,
            &[
                SourceChange::Insert {
                    element: node("known", 1, serde_json::json!({})),
                },
                SourceChange::update(node("unknown", 1, serde_json::json!({}))),
            ],
        )
        .await
        .expect_err("a patch without a retained image must fail");
        assert!(format!("{error:#}").contains("no retained full input image"));
        drop(batch);
        assert!(state
            .committed_input(&ElementReference::new("source", "known"))
            .is_none());

        let future = state.transaction();
        let future_change = SourceChange::Future {
            future_ref: FutureElementRef {
                element_ref: ElementReference::new("source", "known"),
                original_time: 1,
                due_time: 2,
                group_signature: 0,
            },
        };
        assert!(apply_inputs(&future, &[future_change]).await.is_err());
        Ok(())
    }

    fn labelled(id: &str, label: &str, time: u64) -> Element {
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", id),
                labels: vec![Arc::from(label)].into(),
                effective_from: time,
            },
            properties: ElementPropertyMap::from(serde_json::json!({"label": label})),
        }
    }

    #[tokio::test]
    async fn a_filter_retains_only_accepted_images_and_ignores_their_unknown_patches() -> Result<()>
    {
        let state = MemoryState::default();
        let read = |element: &Element| {
            element
                .get_metadata()
                .labels
                .iter()
                .any(|label| &**label == "Read")
        };
        let first = state.transaction();
        apply_inputs_where(
            &first,
            &[
                SourceChange::Insert {
                    element: labelled("kept", "Read", 1),
                },
                SourceChange::Insert {
                    element: labelled("dropped", "Read", 1),
                },
                SourceChange::Insert {
                    element: labelled("other", "Other", 1),
                },
                SourceChange::update(labelled("unknown-other", "Other", 1)),
            ],
            read,
        )
        .await?;
        first.commit();
        assert!(state
            .committed_input(&ElementReference::new("source", "other"))
            .is_none());

        let batch = state.transaction();
        let affected = apply_inputs_where(
            &batch,
            &[
                SourceChange::update(labelled("kept", "Read", 2)),
                SourceChange::Replace {
                    element: labelled("dropped", "Other", 2),
                },
            ],
            read,
        )
        .await?;
        assert_eq!(affected[0].current, Some(labelled("kept", "Read", 2)));
        assert_eq!(
            (affected[1].previous.is_some(), &affected[1].current),
            (true, &None),
            "an image the filter rejects is forgotten"
        );
        let error = apply_inputs_where(
            &batch,
            &[SourceChange::update(labelled("unknown", "Read", 3))],
            read,
        )
        .await
        .expect_err("a patch the filter accepts needs a retained image");
        assert!(format!("{error:#}").contains("no retained full input image"));
        Ok(())
    }

    #[tokio::test]
    async fn a_private_table_reads_back_whole_and_only_its_own_entries() -> Result<()> {
        let state = MemoryState::default();
        let batch = state.transaction();
        for (table, key, value) in [("t", "b", 2), ("u", "a", 9), ("t", "a", 1)] {
            set_value(&batch, table, key, &value).await?;
        }
        batch.delete_value("t", "missing").await?;
        assert_eq!(
            get_values::<u32, _>(&batch, "t").await?,
            [("a".to_owned(), 1), ("b".to_owned(), 2)]
        );
        drop(batch);
        assert!(get_values::<u32, _>(&state.transaction(), "t")
            .await?
            .is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn private_tables_round_trip_values_and_commit_only_with_the_operation() -> Result<()> {
        let state = MemoryState::default();
        let batch = state.transaction();
        set_value(
            &batch,
            "owned",
            "input",
            &vec!["a".to_owned(), "b".to_owned()],
        )
        .await?;
        assert_eq!(
            get_value::<Vec<String>, _>(&batch, "owned", "input").await?,
            Some(vec!["a".to_owned(), "b".to_owned()])
        );
        assert!(state.committed_value("owned", "input").is_none());
        batch.commit();
        assert!(state.committed_value("owned", "input").is_some());
        let removal = state.transaction();
        removal.delete_value("owned", "input").await?;
        assert_eq!(
            get_value::<Vec<String>, _>(&removal, "owned", "input").await?,
            None
        );
        drop(removal);
        assert!(state.committed_value("owned", "input").is_some());
        Ok(())
    }
}
