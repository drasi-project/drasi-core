// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::data::validate_identifier;
use drasi_core_types::models::{Element, SourceChange, SourceMiddlewareConfig};
use std::{collections::BTreeMap, sync::Arc};

#[async_trait]
pub(super) trait Mapper: Send + Sync {
    async fn map(&self, element: Element) -> Result<Vec<Element>>;
}

enum Processor {
    Map(drasi_middleware::map::Map),
    Relabel(drasi_middleware::relabel::RelabelMiddleware),
}

impl Processor {
    async fn apply(&self, element: Element) -> Result<Vec<Element>> {
        let change = SourceChange::Insert { element };
        let changes = match self {
            Self::Map(map) => map.apply(change).await?,
            Self::Relabel(relabel) => relabel.apply(change).await?,
        };
        changes
            .into_iter()
            .map(|change| match change {
                SourceChange::Insert { element } | SourceChange::Replace { element } => Ok(element),
                _ => anyhow::bail!(
                    "view mappings must return complete elements, not event patches or deletions"
                ),
            })
            .collect()
    }
}

pub(super) struct Pipeline {
    steps: Vec<(String, Arc<Processor>)>,
}

impl Pipeline {
    pub(super) fn new(middleware: &[SourceMiddlewareConfig], pipeline: &[String]) -> Result<Self> {
        let mut processors = BTreeMap::new();
        for config in middleware {
            validate_identifier("view middleware", &config.name)?;
            validate_identifier("view middleware kind", &config.kind)?;
            anyhow::ensure!(
                !processors.contains_key(config.name.as_ref()),
                "duplicate view middleware name {}",
                config.name
            );
            let processor = match config.kind.as_ref() {
                "map" => {
                    validate_map(config)?;
                    Processor::Map(drasi_middleware::map::MapFactory::new().build(config)?)
                }
                "relabel" => Processor::Relabel(
                    drasi_middleware::relabel::RelabelMiddlewareFactory::new().build(config)?,
                ),
                kind => anyhow::bail!("view middleware kind {kind} is not registered"),
            };
            processors.insert(config.name.to_string(), Arc::new(processor));
        }
        let steps = pipeline
            .iter()
            .map(|name| {
                validate_identifier("view pipeline step", name)?;
                let processor = processors.get(name).cloned().with_context(|| {
                    format!("view pipeline references undeclared middleware {name}")
                })?;
                Ok((name.clone(), processor))
            })
            .collect::<Result<_>>()?;
        Ok(Self { steps })
    }
}

#[async_trait]
impl Mapper for Pipeline {
    async fn map(&self, element: Element) -> Result<Vec<Element>> {
        let mut elements = vec![element];
        for (name, processor) in &self.steps {
            let mut next = Vec::new();
            for element in elements {
                next.extend(
                    processor
                        .apply(element)
                        .await
                        .with_context(|| format!("view mapping failed in step {name}"))?,
                );
            }
            elements = next;
        }
        Ok(elements)
    }
}

fn validate_map(config: &SourceMiddlewareConfig) -> Result<()> {
    for (label, operations) in &config.config {
        let operations = operations.as_object().with_context(|| {
            format!(
                "view mapping {} for label {label} must be an object",
                config.name
            )
        })?;
        anyhow::ensure!(
            operations.keys().all(|key| key == "insert"),
            "view mapping {} for label {label} accepts only insert rules over complete images",
            config.name
        );
        let rules = operations
            .get("insert")
            .and_then(serde_json::Value::as_array)
            .with_context(|| {
                format!(
                    "view mapping {} for label {label} requires an insert array",
                    config.name
                )
            })?;
        for rule in rules {
            let rule = rule
                .as_object()
                .context("view mapping rule must be an object")?;
            if let Some(operation) = rule.get("op") {
                anyhow::ensure!(
                    matches!(operation.as_str(), Some("Insert" | "Replace")),
                    "view mapping {} must emit complete Insert/Replace images",
                    config.name
                );
            }
        }
    }
    Ok(())
}
