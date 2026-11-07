// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_sdk::{ApplyUpdateRequest, UpdateRequest};

pub(super) struct UpdateState {
    request: ApplyUpdateRequest,
    payload: Vec<u8>,
    preparing: bool,
    prepared: bool,
    operation: Option<OperationFuture>,
    result: Option<std::result::Result<(), Failure>>,
    completed: bool,
}

impl UpdateState {
    pub(super) fn completed_successfully(&self) -> bool {
        self.completed && self.result.as_ref().is_some_and(|result| result.is_ok())
    }
}

impl NativeComponentProxy {
    pub(super) async fn apply_managed_update(
        &mut self,
        context: ComponentUpdateContext,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !context.operation_id.trim().is_empty()
                && context.plan.mode != ComponentUpdateMode::Recreate
                && self
                    .metadata
                    .capabilities
                    .update_modes
                    .contains(&context.plan.mode),
            "native instance cannot apply the requested update mode"
        );
        anyhow::ensure!(
            !self.inner.retiring.load(Ordering::Acquire)
                && !self.inner.stopping.load(Ordering::Acquire),
            "native update requires completed lifecycle work"
        );
        anyhow::ensure!(
            context.current.descriptor == self.descriptor
                && context.proposed.specification.descriptor == self.descriptor
                && context.proposed.component_id == *self.descriptor.id()
                && context.current.dependencies == context.proposed.specification.dependencies,
            "native retained-object update cannot change identity, ports or resource bindings"
        );
        let implementation = if self.query_construction.is_some() {
            ImplementationIdentity::try_new(
                format!("{}/ordinary", self.metadata.implementation.name),
                self.metadata.implementation.version.clone(),
            )?
        } else {
            self.metadata.implementation.clone()
        };
        anyhow::ensure!(
            context.current.implementation == implementation
                && context.proposed.specification.implementation == implementation
                && context.current.configuration_version
                    == context.proposed.specification.configuration_version
                && (self.query_construction.is_some()
                    || context.current.configuration_version
                        == self.metadata.configuration_version),
            "native retained-object update cannot change its factory or configuration version"
        );
        let scope = self
            .construction_scope
            .as_ref()
            .context("native managed update requires its actual construction scope")?;
        anyhow::ensure!(
            scope.instance_id == context.proposed.instance_id.as_ref()
                && scope.graph_id == context.proposed.graph_id.as_ref()
                && scope.generation == context.proposed.generation.0,
            "native update belongs to a different object lifetime"
        );
        let current = self
            .update
            .get_mut()
            .as_ref()
            .filter(|update| update.request.operation_id == context.operation_id.as_ref())
            .map_or_else(
                || self.creation_request.clone(),
                |update| update.request.request.current.clone(),
            );
        let current_configuration = match &self.query_construction {
            Some(binding) => super::super::query::configuration_for_update(
                &context.current,
                Some(binding.construction.provider.is_volatile()),
            )?,
            None => super::super::factory::literal_configuration(&context.current)?,
        };
        anyhow::ensure!(
            current_configuration == current.configuration,
            "native update's previous configuration is stale"
        );
        let mut proposed = current.clone();
        proposed.configuration = match &self.query_construction {
            Some(binding) => super::super::query::configuration_for_update(
                &context.proposed.specification,
                Some(binding.construction.provider.is_volatile()),
            )?,
            None => serde_json::Value::Object(
                context
                    .proposed
                    .configuration()
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone()))
                    .collect(),
            ),
        };
        proposed.update_directive = None;
        if self.metadata.capabilities.binds_inputs {
            proposed.inputs = context.proposed.input_bindings().to_vec();
        }
        self.metadata
            .configuration
            .validate(&proposed.configuration)?;
        let request = ApplyUpdateRequest {
            operation_id: context.operation_id.to_string(),
            request: UpdateRequest { current, proposed },
            plan: context.plan,
        };
        let payload = wire::encode(&request)?;
        if let Some(update) = self.update.get_mut().as_ref() {
            if update.request.operation_id == request.operation_id {
                anyhow::ensure!(
                    update.payload == payload,
                    "native update operation ID was reused for a different request"
                );
            } else {
                anyhow::ensure!(
                    update.completed && update.result.as_ref().is_some_and(|result| result.is_ok()),
                    "native component still owns an unfinished or failed update"
                );
                *self.update.get_mut() = None;
            }
        }
        let update = self.update.get_mut().get_or_insert(UpdateState {
            request,
            payload,
            preparing: false,
            prepared: false,
            operation: None,
            result: None,
            completed: false,
        });
        if update.completed {
            return update
                .result
                .as_ref()
                .expect("completed update")
                .clone()
                .map_err(operation_failure);
        }
        let reopen_processing = update.request.plan.mode == ComponentUpdateMode::Restart;
        if !update.prepared {
            if reopen_processing {
                if !update.preparing {
                    anyhow::ensure!(
                        self.stop.get_mut().completed && self.stop.get_mut().cleanup_complete,
                        "native restart update requires completed stop and cleanup"
                    );
                    update.preparing = true;
                }
                if let Some(processing) = &mut self.processing {
                    self.stop.get_mut().cleanup_complete = false;
                    processing.open().await?;
                }
            }
            if let Some(reads) = &self.output_reads {
                reads.begin_update();
            }
            update.prepared = true;
        }
        if update.result.is_none() {
            if update.operation.is_none() {
                update.operation = Some(self.inner.begin(
                    abi::operation::APPLY_UPDATE,
                    &update.payload,
                    None,
                )?);
            }
            let result = update
                .operation
                .as_mut()
                .expect("owned update operation")
                .await;
            update.operation.take();
            update.result = Some(result.and_then(|bytes| {
                if bytes.is_empty() {
                    Ok(())
                } else {
                    Err(Failure::protocol("native update returned unexpected data"))
                }
            }));
        }
        if reopen_processing {
            if let Some(processing) = &mut self.processing {
                processing.close().await?;
            }
            self.stop.get_mut().cleanup_complete = true;
        }
        if let Err(failure) = update.result.as_ref().expect("finished native update") {
            update.completed = true;
            return Err(operation_failure(failure.clone()));
        }
        let actual = self.inner.inspect()?;
        anyhow::ensure!(
            actual.descriptor == self.descriptor
                && actual.capabilities == self.metadata.capabilities
                && actual.queries.keys().eq(self.query_descriptions.keys()),
            "native update changed an immutable descriptor or capability"
        );
        self.metadata
            .configuration
            .validate(&self.inner.configuration()?)?;
        if let Some(reads) = &self.output_reads {
            reads.refresh_queries(&actual.queries, update.request.plan.data)?;
            reads.finish_update();
            if update.request.plan.mode == ComponentUpdateMode::InPlace {
                reads.recovery_ready();
            }
        }
        self.creation_request = update.request.request.proposed.clone();
        if let Some(binding) = &mut self.query_construction {
            let description = actual
                .queries
                .get(&PortId::try_new("out")?)
                .context("updated ordinary query omitted its output description")?;
            binding.construction.definition.outbox_capacity = description.outbox_capacity;
        }
        self.query_descriptions = actual.queries;
        update.completed = true;
        Ok(())
    }
}
