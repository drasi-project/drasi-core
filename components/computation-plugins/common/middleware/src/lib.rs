// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The one middleware implementation behind the native `drasi.standard/middleware`
//! and `drasi.standard/view` transformers: the declared kinds, the shared
//! `stream`/`middleware`/`pipeline` configuration, and the pipeline that runs
//! the declared steps.
//!
//! The kinds a transformer supports are part of its published configuration
//! schema (a constraint on the `middleware` field), so the graph rejects a
//! definition that names an unsupported kind when it binds the component, not
//! when the plugin constructs it. The kind list is a constant, not whatever the
//! build happened to compile, so Cargo feature unification cannot change what
//! a transformer declares.

#![deny(missing_docs)]

use anyhow::{Context, Result};
use drasi_computation_contracts::{
    data::validate_identifier, ComponentName, ConfigurationConstraint,
};
use drasi_computation_plugin_sdk::{ConfigOption, ConfigType};
use drasi_core_types::models::{Element, SourceChange, SourceMiddlewareConfig};
use drasi_middleware::stateless::{stateless_middleware_factories, StatelessMiddleware};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};

/// The middleware kinds both transformers declare. Every kind is stateless: it
/// never reads stored elements, so the transformer needs no element index.
/// `unwind` reads the previous image of an element and is available only in
/// the query transformer; `jq` needs the system libjq and is available only in
/// query transformer builds that enable it.
pub const STATELESS_KINDS: [&str; 5] = ["decoder", "map", "parse_json", "promote", "relabel"];

/// What a pipeline transforms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Change events, for the stateless middleware transformer. A step may
    /// turn one change into any number of changes of any kind.
    Events,
    /// Complete element images, for the view transformer. Every step must
    /// return complete images, so map declarations accept only insert rules
    /// that emit `Insert` or `Replace`.
    Images,
}

impl Mode {
    /// The kinds this mode declares.
    pub fn kinds(self) -> &'static [&'static str] {
        &STATELESS_KINDS
    }

    /// The JSON Schema of one middleware declaration in this mode.
    fn declaration_schema(self) -> Value {
        let mut schema = ConfigurationConstraint::middleware_declaration(self.kinds());
        if self == Self::Images {
            schema["allOf"] = json!([{
                "if": {"properties": {"kind": {"const": "map"}}},
                "then": {"properties": {"config": {"additionalProperties": {
                    "type": "object",
                    "required": ["insert"],
                    "properties": {"insert": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {"op": {"enum": ["Insert", "Replace"]}},
                        },
                    }},
                    "additionalProperties": false,
                }}}},
            }]);
        }
        schema
    }

    /// The constraint on the `middleware` array that the factory publishes.
    pub fn constraint(self) -> ConfigurationConstraint {
        ConfigurationConstraint::try_new(json!({
            "type": "array",
            "items": self.declaration_schema(),
        }))
        .expect("constant middleware declaration schema")
    }

    /// The `stream`, `middleware` and `pipeline` options, in that order.
    pub fn options(self) -> [ConfigOption; 3] {
        let (stream, images) = match self {
            Self::Events => ("Nonempty output stream identifier.", ""),
            Self::Images => (
                "Nonempty stream identifier for this view's output.",
                " Map declarations accept only insert rules that emit complete Insert or Replace images.",
            ),
        };
        [
            ConfigOption::new("stream", ConfigType::String, true, stream),
            ConfigOption::new(
                "middleware",
                ConfigType::Array,
                true,
                &format!(
                    "Named middleware declarations, each {{name, kind, config}}. kind is one of {}.{images}",
                    self.kinds().join(", ")
                ),
            )
            .constraint(self.constraint()),
            ConfigOption::new(
                "pipeline",
                ConfigType::Array,
                true,
                "Ordered middleware names to apply; every name must refer to a declaration. \
                 An empty pipeline passes its input through unchanged.",
            )
            .constraint(
                ConfigurationConstraint::try_new(json!({"type": "array", "items": name_schema()}))
                    .expect("constant pipeline schema"),
            ),
        ]
    }
}

fn name_schema() -> Value {
    json!({"type": "string", "minLength": 1, "pattern": ComponentName::JSON_SCHEMA_PATTERN})
}

/// The declared steps of one transformer, in pipeline order.
pub struct Pipeline {
    steps: Vec<(String, Arc<dyn StatelessMiddleware>)>,
}

impl Pipeline {
    /// Builds every declaration and resolves the pipeline names. The
    /// declarations are checked against the same constraint the factory
    /// publishes, so construction accepts exactly what binding accepts.
    pub fn new(
        mode: Mode,
        middleware: &[SourceMiddlewareConfig],
        pipeline: &[String],
    ) -> Result<Self> {
        let declaration = ConfigurationConstraint::try_new(mode.declaration_schema())
            .expect("constant middleware declaration schema");
        let factories = stateless_middleware_factories();
        let mut built: BTreeMap<&str, Arc<dyn StatelessMiddleware>> = BTreeMap::new();
        for config in middleware {
            let name = config.name.as_ref();
            validate_identifier("middleware", name)?;
            anyhow::ensure!(
                !built.contains_key(name),
                "duplicate middleware name {name}"
            );
            let kind = config.kind.as_ref();
            anyhow::ensure!(
                mode.kinds().contains(&kind),
                "middleware {name} names kind {kind}, which this transformer does not declare; \
                 declared kinds: {}",
                mode.kinds().join(", ")
            );
            anyhow::ensure!(
                declaration.accepts(&serde_json::to_value(config)?),
                "middleware {name} does not match the declared {kind} configuration{}",
                match (mode, kind) {
                    (Mode::Images, "map") =>
                        "; a view map accepts only insert rules that emit complete Insert or Replace images",
                    _ => "",
                }
            );
            let factory = factories
                .iter()
                .find(|factory| factory.kind() == kind)
                .with_context(|| format!("declared middleware kind {kind} is not compiled in"))?;
            let step = factory
                .build_stateless(config)
                .with_context(|| format!("invalid {kind} middleware {name}"))?;
            built.insert(name, Arc::from(step));
        }
        let steps = pipeline
            .iter()
            .map(|name| {
                validate_identifier("middleware pipeline step", name)?;
                let step = built
                    .get(name.as_str())
                    .cloned()
                    .with_context(|| format!("pipeline references undeclared middleware {name}"))?;
                Ok((name.clone(), step))
            })
            .collect::<Result<_>>()?;
        Ok(Self { steps })
    }

    /// Runs one change event through every step (event mode).
    pub async fn apply(&self, change: SourceChange) -> Result<Vec<SourceChange>> {
        let mut changes = vec![change];
        for (name, step) in &self.steps {
            let mut next = Vec::new();
            for change in changes {
                next.extend(
                    step.apply(change)
                        .await
                        .with_context(|| format!("middleware step {name} failed"))?,
                );
            }
            changes = next;
        }
        Ok(changes)
    }

    /// Maps one complete element image through every step (image mode). Each
    /// step receives the image as an insert and must return complete images.
    pub async fn map(&self, element: Element) -> Result<Vec<Element>> {
        let mut elements = vec![element];
        for (name, step) in &self.steps {
            let mut next = Vec::new();
            for element in elements {
                for change in step
                    .apply(SourceChange::Insert { element })
                    .await
                    .with_context(|| format!("view mapping failed in step {name}"))?
                {
                    next.push(match change {
                        SourceChange::Insert { element } | SourceChange::Replace { element } => {
                            element
                        }
                        _ => anyhow::bail!(
                            "view mapping step {name} must return complete elements, not event \
                             patches or deletions"
                        ),
                    });
                }
            }
            elements = next;
        }
        Ok(elements)
    }
}

#[cfg(test)]
mod tests;
