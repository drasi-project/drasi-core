// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::ComponentUpdatePlan;

#[cfg(test)]
mod tests;

pub(super) fn plan(
    request: &UpdateRequest,
    middleware: &MiddlewareTypeRegistry,
    metadata: &FactoryMetadata,
) -> anyhow::Result<ComponentUpdatePlan> {
    anyhow::ensure!(
        request.current.id == request.proposed.id
            && request.current.implementation == request.proposed.implementation
            && request.current.implementation.name == metadata.implementation.name
            && request.current.implementation.version == metadata.implementation.version
            && metadata
                .implementation
                .plugin
                .as_ref()
                .is_none_or(|plugin| request.current.implementation.plugin.as_ref() == Some(plugin))
            && request.current.configuration_version == metadata.configuration_version
            && request.proposed.configuration_version == metadata.configuration_version
            && request.current.scope == request.proposed.scope
            && request.current.inputs == request.proposed.inputs,
        "query configuration updates cannot change identity, factory, scope or graph bindings"
    );
    metadata
        .configuration
        .validate(&request.current.configuration)?;
    metadata
        .configuration
        .validate(&request.proposed.configuration)?;
    let (current, current_execution) = query_configuration(&request.current, middleware)?;
    let (proposed, proposed_execution) = query_configuration(&request.proposed, middleware)?;
    anyhow::ensure!(
        current.output_stream == proposed.output_stream,
        "query updates cannot change the producer stream"
    );
    let in_place = current.configuration_bytes(&current_execution)?
        == proposed.configuration_bytes(&proposed_execution)?;
    let plan = transformer::update_plan(request, in_place)?;
    if in_place {
        let without_retention = |configuration: &serde_json::Value| -> anyhow::Result<_> {
            let mut fields = configuration
                .as_object()
                .context("query configuration must be an object")?
                .clone();
            fields.remove("outboxCapacity");
            Ok(fields)
        };
        anyhow::ensure!(
            without_retention(&request.current.configuration)?
                == without_retention(&request.proposed.configuration)?,
            "this query update does not implement changes to non-semantic settings other than output retention"
        );
    }
    Ok(plan)
}
