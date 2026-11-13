// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

#[cfg(test)]
mod tests;

fn has_bootstrap(request: &CreateRequest) -> bool {
    match request.implementation.name.as_ref() {
        INDEXED_IMPLEMENTATION => {
            request.configuration.get("bootstrap") == Some(&serde_json::Value::Bool(true))
        }
        PROVIDER_IMPLEMENTATION => true,
        IMPLEMENTATION => request
            .configuration
            .get("bootstrap")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|inputs| !inputs.is_empty()),
        _ => false,
    }
}

pub(super) fn validate_creation(request: &CreateRequest) -> anyhow::Result<()> {
    if let Some(directive) = &request.update_directive {
        directive.validate()?;
        anyhow::ensure!(
            directive.plan.data
                == if has_bootstrap(request) {
                    ComponentUpdateData::Rebootstrap
                } else {
                    ComponentUpdateData::Reset
                },
            "query reconstruction must match its configured snapshot or streaming startup"
        );
    }
    Ok(())
}

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
    for field in [
        "volatile",
        "publication",
        "bootstrap",
        "recovery",
        "reset_configuration",
    ] {
        anyhow::ensure!(
            request.current.configuration.get(field) == request.proposed.configuration.get(field),
            "query updates cannot change the storage, bootstrap or recovery policy"
        );
    }
    if current.configuration_bytes(&current_execution)?
        == proposed.configuration_bytes(&proposed_execution)?
    {
        let without_retention = |configuration: &serde_json::Value| -> anyhow::Result<_> {
            let mut fields = configuration
                .as_object()
                .context("query configuration must be an object")?
                .clone();
            fields.remove("outbox_capacity");
            Ok(fields)
        };
        anyhow::ensure!(
            without_retention(&request.current.configuration)?
                == without_retention(&request.proposed.configuration)?,
            "this query update does not implement changes to non-semantic settings other than output retention"
        );
        anyhow::ensure!(
            proposed.outbox_capacity >= current.outbox_capacity,
            "retained query updates cannot discard history by reducing output retention"
        );
        return Ok(ComponentUpdatePlan {
            mode: ComponentUpdateMode::InPlace,
            data: ComponentUpdateData::Preserve,
        });
    }
    Ok(ComponentUpdatePlan {
        mode: ComponentUpdateMode::Recreate,
        data: if has_bootstrap(&request.proposed) {
            ComponentUpdateData::Rebootstrap
        } else {
            ComponentUpdateData::Reset
        },
    })
}
