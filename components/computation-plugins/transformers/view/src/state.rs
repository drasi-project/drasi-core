// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_computation_contracts::{ElementImage, ElementPatch};
use drasi_core_types::models::{Element, ElementReference, SourceChange};
use std::collections::{HashMap, HashSet};

use crate::mapping::Mapper;

#[derive(Clone)]
pub(super) struct Emission {
    owner: ElementReference,
    pub(super) image: ElementImage,
}

pub(super) type Rows = im::HashMap<ElementReference, Emission>;

#[derive(Clone, Default)]
pub(super) struct State {
    inputs: im::HashMap<ElementReference, Element>,
    associations: im::HashMap<ElementReference, Vec<ElementReference>>,
    pub(super) outputs: Rows,
}

impl State {
    pub(super) async fn prepare(
        &self,
        changes: Vec<SourceChange>,
        mapper: &dyn Mapper,
    ) -> Result<(Self, Vec<SourceChange>)> {
        let mut next = self.clone();
        let mut touched = Vec::new();
        let mut times = HashMap::new();
        for change in changes {
            change.validate()?;
            let metadata = match &change {
                SourceChange::Insert { element }
                | SourceChange::Replace { element }
                | SourceChange::Update { element, .. } => element.get_metadata(),
                SourceChange::Delete { metadata } => metadata,
                SourceChange::Future { .. } => {
                    anyhow::bail!("view transforms do not accept scheduled notifications")
                }
            };
            let reference = metadata.reference.clone();
            if times
                .insert(reference.clone(), metadata.effective_from)
                .is_none()
            {
                touched.push(reference.clone());
            }
            match change {
                SourceChange::Insert { element } | SourceChange::Replace { element } => {
                    next.inputs.insert(reference, element);
                }
                SourceChange::Update {
                    element,
                    removed_properties,
                } => {
                    let base = next.inputs.get(&reference).with_context(|| {
                        format!("view patch for {reference} has no retained full input image")
                    })?;
                    let patch = ElementPatch::try_new(
                        ElementImage::try_from(element)?,
                        removed_properties,
                    )?;
                    next.inputs.insert(reference, patch.apply_to(Some(base))?);
                }
                SourceChange::Delete { .. } => {
                    next.inputs.remove(&reference);
                }
                SourceChange::Future { .. } => {
                    anyhow::bail!("view transforms do not accept scheduled notifications")
                }
            }
        }

        let mut keys = Vec::new();
        let mut seen = HashSet::new();
        // Release all affected ownership first, so a batch may exchange keys.
        for reference in &touched {
            if let Some(previous) = next.associations.remove(reference) {
                for key in previous {
                    next.outputs.remove(&key);
                    remember(&key, &mut keys, &mut seen);
                }
            }
        }
        for reference in touched {
            let Some(element) = next.inputs.get(&reference) else {
                continue;
            };
            let mapped = mapper
                .map(element.clone())
                .await
                .with_context(|| format!("view mapping failed for input {reference}"))?;
            let mut associations = Vec::with_capacity(mapped.len());
            for element in mapped {
                let key = element.get_reference().clone();
                if let Some(existing) = next.outputs.get(&key) {
                    anyhow::bail!(
                        "view output identity {key} is emitted more than once (inputs {} and {reference})",
                        existing.owner
                    );
                }
                remember(&key, &mut keys, &mut seen);
                associations.push(key.clone());
                next.outputs.insert(
                    key,
                    Emission {
                        owner: reference.clone(),
                        image: ElementImage::try_from(element)?,
                    },
                );
            }
            next.associations.insert(reference, associations);
        }

        let mut removals = Vec::new();
        let mut updates = Vec::new();
        for key in keys {
            let previous = self.outputs.get(&key);
            let current = next.outputs.get(&key);
            let same_kind = match (previous, current) {
                (Some(previous), Some(current)) => {
                    std::mem::discriminant(&previous.image)
                        == std::mem::discriminant(&current.image)
                }
                _ => false,
            };
            if let Some(previous) = previous {
                if !same_kind {
                    let mut metadata = previous.image.metadata().clone();
                    metadata.effective_from = *times
                        .get(&previous.owner)
                        .context("view retraction has no triggering input")?;
                    removals.push(SourceChange::Delete { metadata });
                }
            }
            if let Some(current) = current {
                if !same_kind {
                    updates.push(SourceChange::Insert {
                        element: Element::try_from(&current.image)?,
                    });
                } else if previous.is_some_and(|previous| previous.image != current.image) {
                    updates.push(SourceChange::Replace {
                        element: Element::try_from(&current.image)?,
                    });
                }
            }
        }
        removals.extend(updates);
        Ok((next, removals))
    }
}

fn remember(
    reference: &ElementReference,
    ordered: &mut Vec<ElementReference>,
    seen: &mut HashSet<ElementReference>,
) {
    if seen.insert(reference.clone()) {
        ordered.push(reference.clone());
    }
}
