// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    computation::v1::ComponentHandle,
    config::{ComponentId, ComponentName, ComponentReference},
    DrasiLib, DrasiLibBuilder, Reaction, Source,
};
use std::collections::HashMap;

pub(crate) struct SourceFixture<S> {
    name: ComponentName,
    auto_start: bool,
    lifecycle: Option<Box<dyn Fn() -> bool + Send + Sync>>,
    create: Box<dyn FnOnce(ComponentId) -> anyhow::Result<S> + Send>,
}

impl<S> SourceFixture<S> {
    pub(crate) fn new(
        name: ComponentName,
        auto_start: bool,
        create: impl FnOnce(ComponentId) -> anyhow::Result<S> + Send + 'static,
    ) -> Self {
        Self {
            name,
            auto_start,
            lifecycle: None,
            create: Box::new(create),
        }
    }

    pub(crate) fn auto_start(&self) -> bool {
        self.lifecycle
            .as_ref()
            .map_or(self.auto_start, |read| read())
    }

    pub(crate) fn construct(self, id: ComponentId) -> anyhow::Result<S> {
        (self.create)(id)
    }

    pub(crate) fn name(&self) -> &ComponentName {
        &self.name
    }

    pub(crate) fn with_lifecycle(
        mut self,
        read: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        self.lifecycle = Some(Box::new(read));
        self
    }

    pub(crate) fn map(mut self, map: impl FnOnce(S) -> S + Send + 'static) -> Self
    where
        S: 'static,
    {
        let create = self.create;
        self.create = Box::new(move |id| create(id).map(map));
        self
    }
}

pub(crate) struct ReactionFixture<R> {
    name: ComponentName,
    inputs: Vec<ComponentReference>,
    auto_start: bool,
    create: Box<dyn FnOnce(ComponentId, Vec<ComponentId>) -> anyhow::Result<R> + Send>,
}

impl<R> ReactionFixture<R> {
    pub(crate) fn new(
        name: ComponentName,
        inputs: Vec<ComponentReference>,
        auto_start: bool,
        create: impl FnOnce(ComponentId, Vec<ComponentId>) -> anyhow::Result<R> + Send + 'static,
    ) -> Self {
        Self {
            name,
            inputs,
            auto_start,
            create: Box::new(create),
        }
    }
    pub(crate) fn auto_start(&self) -> bool {
        self.auto_start
    }
    pub(crate) fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }
    pub(crate) fn construct(self, id: ComponentId, inputs: Vec<ComponentId>) -> anyhow::Result<R> {
        (self.create)(id, inputs)
    }
}

pub(crate) trait BuilderFixtures {
    fn with_source_fixture<S: Source + 'static>(self, fixture: SourceFixture<S>) -> Self;
    fn with_source_fixture_metadata<S: Source + 'static>(
        self,
        fixture: SourceFixture<S>,
        metadata: HashMap<String, String>,
    ) -> Self;
    fn with_reaction_fixture<R: Reaction + 'static>(self, fixture: ReactionFixture<R>) -> Self;
}

impl BuilderFixtures for DrasiLibBuilder {
    fn with_source_fixture<S: Source + 'static>(self, fixture: SourceFixture<S>) -> Self {
        let auto_start = fixture.auto_start();
        self.with_source_factory(fixture.name, auto_start, fixture.create)
    }

    fn with_reaction_fixture<R: Reaction + 'static>(self, fixture: ReactionFixture<R>) -> Self {
        self.with_reaction_inputs_factory(
            fixture.name,
            fixture.inputs,
            fixture.auto_start,
            fixture.create,
        )
    }

    fn with_source_fixture_metadata<S: Source + 'static>(
        self,
        fixture: SourceFixture<S>,
        metadata: HashMap<String, String>,
    ) -> Self {
        let auto_start = fixture.auto_start();
        self.with_source_factory_async(fixture.name, None, auto_start, move |id| async move {
            Ok(((fixture.create)(id)?, metadata))
        })
    }
}

pub(crate) trait CoreFixtures {
    async fn add_source_fixture<S: Source + 'static>(
        &self,
        fixture: SourceFixture<S>,
    ) -> crate::Result<()>;
    async fn add_source_fixture_with_handle<S: Source + 'static>(
        &self,
        fixture: SourceFixture<S>,
    ) -> crate::Result<ComponentHandle>;
    async fn add_source_fixture_with_metadata<S: Source + 'static>(
        &self,
        fixture: SourceFixture<S>,
        metadata: HashMap<String, String>,
    ) -> crate::Result<()>;
    async fn replace_source_fixture<S: Source + 'static>(
        &self,
        reference: ComponentReference,
        fixture: SourceFixture<S>,
    ) -> crate::Result<()>;
    async fn add_reaction_fixture<R: Reaction + 'static>(
        &self,
        fixture: ReactionFixture<R>,
    ) -> crate::Result<()>;
    async fn add_reaction_fixture_with_handle<R: Reaction + 'static>(
        &self,
        fixture: ReactionFixture<R>,
    ) -> crate::Result<ComponentHandle>;
    async fn add_reaction_fixture_with_metadata<R: Reaction + 'static>(
        &self,
        fixture: ReactionFixture<R>,
        metadata: HashMap<String, String>,
    ) -> crate::Result<()>;
    async fn replace_reaction_fixture<R: Reaction + 'static>(
        &self,
        reference: ComponentReference,
        fixture: ReactionFixture<R>,
    ) -> crate::Result<()>;
}

impl CoreFixtures for DrasiLib {
    async fn add_source_fixture<S: Source + 'static>(
        &self,
        fixture: SourceFixture<S>,
    ) -> crate::Result<()> {
        self.add_source_fixture_with_handle(fixture)
            .await
            .map(|_| ())
    }

    async fn add_source_fixture_with_handle<S: Source + 'static>(
        &self,
        fixture: SourceFixture<S>,
    ) -> crate::Result<ComponentHandle> {
        let auto_start = fixture.auto_start();
        self.create_source(fixture.name, auto_start, fixture.create)
            .await
    }

    async fn add_source_fixture_with_metadata<S: Source + 'static>(
        &self,
        fixture: SourceFixture<S>,
        metadata: HashMap<String, String>,
    ) -> crate::Result<()> {
        let auto_start = fixture.auto_start();
        self.create_source_async(fixture.name, None, auto_start, move |id| async move {
            Ok(((fixture.create)(id)?, metadata))
        })
        .await
        .map(|_| ())
    }

    async fn replace_source_fixture<S: Source + 'static>(
        &self,
        reference: ComponentReference,
        fixture: SourceFixture<S>,
    ) -> crate::Result<()> {
        let id = fixture_identity(self, reference)?;
        self.remove_source_by_id(&id, false).await?;
        self.add_source_fixture(fixture).await
    }

    async fn add_reaction_fixture<R: Reaction + 'static>(
        &self,
        fixture: ReactionFixture<R>,
    ) -> crate::Result<()> {
        self.add_reaction_fixture_with_handle(fixture)
            .await
            .map(|_| ())
    }

    async fn add_reaction_fixture_with_handle<R: Reaction + 'static>(
        &self,
        fixture: ReactionFixture<R>,
    ) -> crate::Result<ComponentHandle> {
        self.create_reaction_with_inputs(
            fixture.name,
            fixture.inputs,
            fixture.auto_start,
            fixture.create,
        )
        .await
    }

    async fn add_reaction_fixture_with_metadata<R: Reaction + 'static>(
        &self,
        fixture: ReactionFixture<R>,
        metadata: HashMap<String, String>,
    ) -> crate::Result<()> {
        self.create_reaction_async(
            fixture.name,
            None,
            fixture.inputs,
            fixture.auto_start,
            move |id, inputs| async move { Ok(((fixture.create)(id, inputs)?, metadata)) },
        )
        .await
        .map(|_| ())
    }

    async fn replace_reaction_fixture<R: Reaction + 'static>(
        &self,
        reference: ComponentReference,
        fixture: ReactionFixture<R>,
    ) -> crate::Result<()> {
        let id = fixture_identity(self, reference)?;
        self.remove_reaction_by_id(&id, false).await?;
        self.add_reaction_fixture(fixture).await
    }
}

fn fixture_identity(core: &DrasiLib, reference: ComponentReference) -> crate::Result<ComponentId> {
    if let Some(id) = reference.as_id() {
        return Ok(id.clone());
    }
    let name = reference.as_name().expect("explicit name reference");
    Ok(core.component_by_name(name)?.id().clone())
}
