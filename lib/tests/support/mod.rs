// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![allow(dead_code)]

use drasi_lib::{
    computation::v1::{
        ComponentBatchBuilder, ComponentFactory, ComponentSpecification, ComputationGraphBuilder,
        ComputationService, EnvelopeSink, EnvelopeSource, Transformer,
    },
    config::{ComponentId, ComponentName},
    DrasiLibBuilder, Reaction, Source,
};
use std::sync::atomic::{AtomicU64, Ordering};

/// Stable fixture UUID selected independently of the component's name.
pub fn component_id(value: u128) -> ComponentId {
    ComponentId::from_uuid(uuid::Uuid::from_u128(value))
}

/// Allocate an independent name for cases that do not exercise name selection.
pub fn component_name() -> ComponentName {
    static NEXT_NAME: AtomicU64 = AtomicU64::new(1);
    ComponentName::try_new(format!(
        "fixture-{}",
        NEXT_NAME.fetch_add(1, Ordering::Relaxed)
    ))
    .expect("valid fixture component name")
}

/// Adds low-level test components whose editable names are irrelevant to the
/// assertion. Tests that exercise naming must call the production builder
/// methods with an explicit `ComponentName` instead.
pub trait FixtureGraphBuilderExt: Sized {
    fn fixture_component(
        self,
        specification: ComponentSpecification,
        factory: std::sync::Arc<dyn ComponentFactory>,
    ) -> Self;
    fn fixture_source(self, source: Box<dyn EnvelopeSource>) -> Self;
    fn fixture_transformer(self, transformer: Box<dyn Transformer>) -> Self;
    fn fixture_sink(self, sink: Box<dyn EnvelopeSink>) -> Self;
    fn fixture_service(self, service: Box<dyn ComputationService>) -> Self;
}

impl FixtureGraphBuilderExt for ComputationGraphBuilder {
    fn fixture_component(
        self,
        specification: ComponentSpecification,
        factory: std::sync::Arc<dyn ComponentFactory>,
    ) -> Self {
        let name = component_name();
        self.component(name, specification, factory)
    }

    fn fixture_source(self, source: Box<dyn EnvelopeSource>) -> Self {
        let name = component_name();
        self.source(name, source)
    }

    fn fixture_transformer(self, transformer: Box<dyn Transformer>) -> Self {
        let name = component_name();
        self.transformer(name, transformer)
    }

    fn fixture_sink(self, sink: Box<dyn EnvelopeSink>) -> Self {
        let name = component_name();
        self.sink(name, sink)
    }

    fn fixture_service(self, service: Box<dyn ComputationService>) -> Self {
        let name = component_name();
        self.service(name, service)
    }
}

impl FixtureGraphBuilderExt for ComponentBatchBuilder {
    fn fixture_component(
        self,
        specification: ComponentSpecification,
        factory: std::sync::Arc<dyn ComponentFactory>,
    ) -> Self {
        let name = component_name();
        self.component(name, specification, factory)
    }

    fn fixture_source(self, source: Box<dyn EnvelopeSource>) -> Self {
        let name = component_name();
        self.source(name, source)
    }

    fn fixture_transformer(self, transformer: Box<dyn Transformer>) -> Self {
        let name = component_name();
        self.transformer(name, transformer)
    }

    fn fixture_sink(self, sink: Box<dyn EnvelopeSink>) -> Self {
        let name = component_name();
        self.sink(name, sink)
    }

    fn fixture_service(self, service: Box<dyn ComputationService>) -> Self {
        let name = component_name();
        self.service(name, service)
    }
}

/// Ordinary fixtures declare their name separately from their immutable ID.
pub trait FixtureDrasiLibBuilderExt: Sized {
    fn with_fixture_source<S: Source + 'static>(self, name: ComponentName, source: S) -> Self;
    fn with_fixture_reaction<R: Reaction + 'static>(self, name: ComponentName, reaction: R)
        -> Self;
}

impl FixtureDrasiLibBuilderExt for DrasiLibBuilder {
    fn with_fixture_source<S: Source + 'static>(self, name: ComponentName, source: S) -> Self {
        self.with_source(name, source)
    }

    fn with_fixture_reaction<R: Reaction + 'static>(
        self,
        name: ComponentName,
        reaction: R,
    ) -> Self {
        self.with_reaction(name, reaction)
    }
}
