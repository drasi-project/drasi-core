// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::HashMap, sync::OnceLock};
use tokio::sync::watch;

use crate::computation::v1::{ComponentGeneration, ComponentId, GraphControl, PluginIdentity};
use crate::error::RetainedError;

pub(super) fn publish_metadata(
    metadata: Option<&HashMap<String, String>>,
    control: &GraphControl,
    id: &ComponentId,
    generation: ComponentGeneration,
) -> anyhow::Result<()> {
    if let Some((plugin, version)) =
        metadata.and_then(|metadata| metadata.get("pluginId").zip(metadata.get("pluginVersion")))
    {
        control.observe_plugin(
            id,
            generation,
            PluginIdentity {
                id: plugin.as_str().into(),
                version: version.as_str().into(),
            },
        )?;
    }
    Ok(())
}

type Outcome = std::result::Result<(), RetainedError>;

pub(super) struct ConstructionState {
    outcome: watch::Sender<Option<Outcome>>,
    pub(super) metadata: OnceLock<HashMap<String, String>>,
}

impl ConstructionState {
    pub(super) fn new() -> Self {
        Self {
            outcome: watch::channel(None).0,
            metadata: OnceLock::new(),
        }
    }

    pub(super) fn outcome(&self) -> Option<anyhow::Result<()>> {
        self.outcome
            .borrow()
            .clone()
            .map(|result| result.map_err(anyhow::Error::from))
    }

    pub(super) fn guard(&self) -> ConstructionGuard<'_> {
        ConstructionGuard {
            state: self,
            complete: false,
        }
    }

    pub(super) async fn wait(&self) -> anyhow::Result<()> {
        let mut changes = self.outcome.subscribe();
        loop {
            if let Some(result) = changes.borrow_and_update().clone() {
                return result.map_err(Into::into);
            }
            changes.changed().await?;
        }
    }
}

pub(super) struct ConstructionGuard<'a> {
    state: &'a ConstructionState,
    complete: bool,
}

impl ConstructionGuard<'_> {
    pub(super) fn finish(mut self, result: anyhow::Result<()>) -> anyhow::Result<()> {
        let result = result.map_err(RetainedError::from);
        self.state.outcome.send_replace(Some(result.clone()));
        self.complete = true;
        result.map_err(Into::into)
    }
}

impl Drop for ConstructionGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.state
                .outcome
                .send_replace(Some(Err(RetainedError::from(anyhow::anyhow!(
                    "ordinary component construction was interrupted; replace the declaration"
                )))));
        }
    }
}
