// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The view's dataset semantics over its durable transformer state: the
//! retained inputs, the outputs each input produced and each output's owner.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use drasi_computation_contracts::ElementImage;
use drasi_computation_plugin_sdk::transformer::{self, StateStore};
use drasi_core_types::models::{Element, ElementReference, SourceChange};
use serde::{Deserialize, Serialize};

use crate::mapping::Mapper;

/// The output identities each input produced, keyed by input.
const OWNED: &str = "view/owned";
/// Each current output's owning input and projected image, keyed by output.
const OUTPUTS: &str = "view/outputs";

#[derive(Clone, Serialize, Deserialize)]
struct Emission {
    owner: ElementReference,
    image: ElementImage,
}

fn key(reference: &ElementReference) -> Result<String> {
    Ok(serde_json::to_string(&(
        reference.namespace.as_ref(),
        reference.element_id.as_ref(),
    ))?)
}

/// Apply one input envelope's changes and return the net changes to the view.
/// Every staged write belongs to the caller's operation: an error leaves the
/// committed state untouched.
pub(super) async fn transform(
    store: &dyn StateStore,
    changes: &[SourceChange],
    mapper: &dyn Mapper,
) -> Result<Vec<SourceChange>> {
    let inputs = transformer::apply_inputs(store, changes).await?;
    let times: HashMap<_, _> = inputs
        .iter()
        .map(|input| (input.reference.clone(), input.effective_from))
        .collect();
    // Committed emissions of every output this batch touches, and their staged
    // replacements (`None` once released).
    let mut previous: HashMap<ElementReference, Option<Emission>> = HashMap::new();
    let mut staged: HashMap<ElementReference, Option<Emission>> = HashMap::new();
    let mut keys = Vec::new();
    let mut seen = HashSet::new();

    // Release all affected ownership first, so a batch may exchange keys.
    for input in &inputs {
        let owned: Vec<ElementReference> =
            transformer::get_value(store, OWNED, &key(&input.reference)?)
                .await?
                .unwrap_or_default();
        for output in owned {
            committed(store, &mut previous, &output).await?;
            staged.insert(output.clone(), None);
            remember(&output, &mut keys, &mut seen);
        }
    }
    let mut ownership = Vec::with_capacity(inputs.len());
    for input in &inputs {
        let Some(element) = &input.current else {
            ownership.push((input.reference.clone(), Vec::new()));
            continue;
        };
        let mapped = mapper
            .map(element.clone())
            .await
            .with_context(|| format!("view mapping failed for input {}", input.reference))?;
        let mut owned = Vec::with_capacity(mapped.len());
        for element in mapped {
            let output = element.get_reference().clone();
            let existing = match staged.get(&output) {
                Some(emission) => emission.clone(),
                None => committed(store, &mut previous, &output).await?,
            };
            if let Some(existing) = existing {
                anyhow::bail!(
                    "view output identity {output} is emitted more than once (inputs {} and {})",
                    existing.owner,
                    input.reference
                );
            }
            remember(&output, &mut keys, &mut seen);
            owned.push(output.clone());
            staged.insert(
                output,
                Some(Emission {
                    owner: input.reference.clone(),
                    image: ElementImage::try_from(element)?,
                }),
            );
        }
        ownership.push((input.reference.clone(), owned));
    }

    let mut removals = Vec::new();
    let mut updates = Vec::new();
    for output in &keys {
        let previous = previous.get(output).cloned().flatten();
        let current = staged.get(output).cloned().flatten();
        let same_kind = match (&previous, &current) {
            (Some(previous), Some(current)) => {
                std::mem::discriminant(&previous.image) == std::mem::discriminant(&current.image)
            }
            _ => false,
        };
        if let Some(previous) = &previous {
            if !same_kind {
                let mut metadata = previous.image.metadata().clone();
                metadata.effective_from = *times
                    .get(&previous.owner)
                    .context("view retraction has no triggering input")?;
                removals.push(SourceChange::Delete { metadata });
            }
        }
        if let Some(current) = &current {
            if !same_kind {
                updates.push(SourceChange::Insert {
                    element: Element::try_from(&current.image)?,
                });
            } else if previous
                .as_ref()
                .is_some_and(|previous| previous.image != current.image)
            {
                updates.push(SourceChange::Replace {
                    element: Element::try_from(&current.image)?,
                });
            }
        }
    }

    for (input, owned) in ownership {
        if owned.is_empty() {
            store.delete_value(OWNED, &key(&input)?).await?;
        } else {
            transformer::set_value(store, OWNED, &key(&input)?, &owned).await?;
        }
    }
    for output in &keys {
        match staged.get(output).cloned().flatten() {
            Some(emission) => {
                transformer::set_value(store, OUTPUTS, &key(output)?, &emission).await?
            }
            None => store.delete_value(OUTPUTS, &key(output)?).await?,
        }
    }
    removals.extend(updates);
    Ok(removals)
}

/// The committed emission of `output`, read once per batch.
async fn committed(
    store: &dyn StateStore,
    previous: &mut HashMap<ElementReference, Option<Emission>>,
    output: &ElementReference,
) -> Result<Option<Emission>> {
    if let Some(emission) = previous.get(output) {
        return Ok(emission.clone());
    }
    let emission: Option<Emission> = transformer::get_value(store, OUTPUTS, &key(output)?).await?;
    previous.insert(output.clone(), emission.clone());
    Ok(emission)
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

/// The committed images of the view's durable output records.
#[cfg(test)]
pub(super) fn committed_outputs(
    state: &transformer::MemoryState,
) -> Result<HashMap<ElementReference, Element>> {
    state
        .committed_values()
        .into_iter()
        .filter(|(table, _, _)| table == OUTPUTS)
        .map(|(_, _, value)| {
            let emission: Emission = serde_json::from_str(&value)?;
            let element = Element::try_from(&emission.image)?;
            Ok((element.get_reference().clone(), element))
        })
        .collect()
}
