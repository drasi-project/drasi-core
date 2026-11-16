// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_sdk::{wire::CreateRequest, ApplyUpdateRequest, UpdateRequest};

pub(super) struct UpdateState {
    request: ApplyUpdateRequest,
    preparing: bool,
    prepared: bool,
    operation: Option<OperationFuture>,
    result: Option<std::result::Result<(), Failure>>,
    completed: bool,
}

impl UpdateState {
    fn succeeded(&self) -> bool {
        self.completed && self.result.as_ref().is_some_and(|result| result.is_ok())
    }

    /// A finished update that left the object usable: it succeeded, or the
    /// plugin reported a refusal or failure that did not change the object.
    pub(super) fn permits_activation(&self) -> bool {
        self.completed
            && self
                .result
                .as_ref()
                .is_some_and(|result| result.as_ref().map_or_else(apply_unchanged, |()| true))
    }
}

/// The SDK's encoding of an apply outcome in the failure data.
fn apply_outcome(failure: &Failure) -> Option<&str> {
    if failure.code != abi::status::FAILED {
        return None;
    }
    failure.data.as_ref()?.get("apply")?.as_str()
}

fn apply_unchanged(failure: &Failure) -> bool {
    matches!(apply_outcome(failure), Some("refused" | "failed"))
}

/// Map a finished native APPLY failure. An interrupted, cancelled or
/// unclassified apply cannot be proven harmless, so it damages the object.
fn apply_error(failure: Failure) -> ApplyError {
    match apply_outcome(&failure) {
        Some("refused") => ApplyError::Refused {
            now_requires: failure
                .data
                .as_ref()
                .and_then(|data| data.get("nowRequires"))
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .flatten(),
            reason: failure.message,
        },
        Some("failed") => ApplyError::Failed(operation_failure(failure)),
        _ => ApplyError::Damaged(operation_failure(failure)),
    }
}

/// Unwrap a host-side step whose failure leaves the native object unchanged.
macro_rules! unchanged {
    ($value:expr) => {
        $value.map_err(ApplyError::Failed)?
    };
}

impl NativeComponentProxy {
    fn update_implementation(&self) -> anyhow::Result<ImplementationIdentity> {
        Ok(if self.query_construction.is_some() {
            crate::computation::query::continuous_query_implementation(
                &self.metadata.implementation,
            )?
        } else {
            self.metadata.implementation.clone()
        })
    }

    fn update_configuration(
        &self,
        specification: &ComponentSpecification,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        Ok(match &self.query_construction {
            Some(binding) => Some(super::super::query::configuration_for_update(
                specification,
                Some(binding.construction.provider.is_volatile()),
            )?),
            None => None,
        })
    }

    /// The structural checks and native description shared by assessment and
    /// apply. `proposed_configuration` is the resolved literal configuration.
    fn update_request(
        &self,
        base: &CreateRequest,
        current: &ComponentSpecification,
        proposed: &ComponentSpecification,
        proposed_configuration: serde_json::Value,
        inputs: Option<&[InputBinding]>,
    ) -> anyhow::Result<UpdateRequest> {
        anyhow::ensure!(
            current.descriptor == self.descriptor
                && proposed.descriptor == self.descriptor
                && current.dependencies == proposed.dependencies,
            "native retained-object update cannot change identity, ports or resource bindings"
        );
        let implementation = self.update_implementation()?;
        anyhow::ensure!(
            current.implementation == implementation
                && proposed.implementation == implementation
                && current.configuration_version == proposed.configuration_version
                && (self.query_construction.is_some()
                    || current.configuration_version == self.metadata.configuration_version),
            "native retained-object update cannot change its factory or configuration version"
        );
        let current_configuration = match self.update_configuration(current)? {
            Some(configuration) => configuration,
            None => super::super::factory::literal_configuration(current)?,
        };
        anyhow::ensure!(
            current_configuration == base.configuration,
            "native update's previous configuration is stale"
        );
        let mut next = base.clone();
        next.configuration = match self.update_configuration(proposed)? {
            Some(configuration) => configuration,
            None => proposed_configuration,
        };
        next.update_directive = None;
        if let Some(inputs) = inputs.filter(|_| self.metadata.capabilities.binds_inputs) {
            next.inputs = inputs.to_vec();
        }
        self.metadata.configuration.validate(&next.configuration)?;
        Ok(UpdateRequest {
            current: base.clone(),
            proposed: next,
        })
    }

    /// Ask the live native object to assess a change. Unsupported, busy and
    /// unresolved-reference changes defer to the factory planner.
    pub(super) fn assess_managed_update(
        &self,
        current: &ComponentSpecification,
        proposed: &ComponentSpecification,
    ) -> anyhow::Result<Option<Assessment>> {
        let Some(management) = &self.management else {
            return Ok(None);
        };
        if self.inner.retiring.load(Ordering::Acquire)
            || self.inner.stopping.load(Ordering::Acquire)
        {
            return Ok(None);
        }
        let configuration = if self.query_construction.is_some() {
            serde_json::Value::Null
        } else {
            match super::super::factory::literal_configuration(proposed) {
                Ok(configuration) => configuration,
                Err(_) => return Ok(None),
            }
        };
        let base = match self.update.try_lock() {
            Ok(update) => match update.as_ref() {
                Some(update) if !update.succeeded() => return Ok(None),
                _ => self.creation_request.clone(),
            },
            Err(_) => return Ok(None),
        };
        let request = self.update_request(&base, current, proposed, configuration, None)?;
        let Some(assessment) = self.assess_request(management, &request)? else {
            return Ok(None);
        };
        *self
            .assessment
            .lock()
            .map_err(|_| anyhow::anyhow!("native assessment cache is poisoned"))? =
            Some((request, assessment.clone()));
        Ok(Some(assessment))
    }

    fn assess_request(
        &self,
        management: &abi::management::PluginManagementV1,
        request: &UpdateRequest,
    ) -> anyhow::Result<Option<Assessment>> {
        let payload = serde_json::to_vec(request)?;
        anyhow::ensure!(
            payload.len() <= abi::MAX_METADATA_BYTES,
            "native update request exceeds size limit"
        );
        let response = unsafe {
            take_reply(management.assess.expect("validated management")(
                self.inner.raw.state,
                abi::BorrowedBytes::new(&payload),
            ))
        };
        let response = match response {
            Ok(response) => response,
            Err(failure)
                if failure.code == abi::status::UNSUPPORTED
                    || failure.code == abi::status::BUSY =>
            {
                return Ok(None);
            }
            Err(failure) => return Err(operation_failure(failure)),
        };
        anyhow::ensure!(
            response.len() <= abi::MAX_METADATA_BYTES,
            "native assessment exceeds size limit"
        );
        let assessment: Assessment = serde_json::from_slice(&response)?;
        if let Some(plan) = assessment.plan() {
            anyhow::ensure!(
                self.metadata.capabilities.update_modes.contains(&plan.mode),
                "native instance returned an undeclared update boundary"
            );
        }
        Ok(Some(assessment))
    }

    /// The fingerprint of the live assessment this exact update was accepted
    /// from. Without a matching assessment the native recheck decides.
    fn accepted_fingerprint(
        &self,
        request: &UpdateRequest,
        plan: ComponentUpdatePlan,
    ) -> anyhow::Result<Option<String>> {
        let cached = self
            .assessment
            .lock()
            .map_err(|_| anyhow::anyhow!("native assessment cache is poisoned"))?
            .take();
        let assessment = match cached {
            Some((assessed, assessment)) if assessed == *request => Some(assessment),
            _ => match &self.management {
                Some(management) => self.assess_request(management, request)?,
                None => None,
            },
        };
        Ok(assessment
            .filter(|assessment| assessment.plan() == Some(plan))
            .and_then(|assessment| assessment.fingerprint().map(str::to_owned)))
    }

    pub(super) async fn apply_managed_update(
        &mut self,
        apply: &ApplyContext,
        context: ComponentUpdateContext,
    ) -> std::result::Result<(), ApplyError> {
        unchanged!(self.prepare_managed_update(apply, &context));
        let operation_id = context.operation_id.to_string();
        let existing = self
            .update
            .get_mut()
            .as_ref()
            .filter(|update| update.request.operation_id == operation_id)
            .map(|update| update.request.request.current.clone());
        let base = existing
            .clone()
            .unwrap_or_else(|| self.creation_request.clone());
        let request = unchanged!(self.update_request(
            &base,
            &context.current,
            &context.proposed.specification,
            serde_json::Value::Object(
                context
                    .proposed
                    .configuration()
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone()))
                    .collect(),
            ),
            Some(context.proposed.input_bindings()),
        ));
        if let Some(update) = self.update.get_mut().as_ref() {
            if existing.is_some() {
                if update.request.request != request || update.request.plan != context.plan {
                    return Err(ApplyError::Failed(anyhow::anyhow!(
                        "native update operation ID was reused for a different request"
                    )));
                }
            } else {
                if !update.permits_activation() {
                    return Err(ApplyError::Failed(anyhow::anyhow!(
                        "native component still owns an unfinished or damaging update"
                    )));
                }
                *self.update.get_mut() = None;
            }
        }
        if self.update.get_mut().is_none() {
            let fingerprint = unchanged!(self.accepted_fingerprint(&request, context.plan));
            *self.update.get_mut() = Some(UpdateState {
                request: ApplyUpdateRequest {
                    operation_id,
                    request,
                    plan: context.plan,
                    fingerprint,
                },
                preparing: false,
                prepared: false,
                operation: None,
                result: None,
                completed: false,
            });
        }
        let lifecycle = self.lifecycle_request(apply.scope());
        let update = self.update.get_mut().as_mut().expect("owned update");
        if update.completed {
            return match update.result.as_ref().expect("completed update") {
                Ok(()) => Ok(()),
                Err(failure) => Err(apply_error(failure.clone())),
            };
        }
        let reopen_processing = update.request.plan.mode == ComponentUpdateMode::Restart;
        if !update.prepared {
            if reopen_processing {
                if !update.preparing {
                    if !(self.stop.get_mut().completed && self.stop.get_mut().cleanup_complete) {
                        return Err(ApplyError::Failed(anyhow::anyhow!(
                            "native restart update requires completed stop and cleanup"
                        )));
                    }
                    update.preparing = true;
                }
                if let Some(processing) = &mut self.processing {
                    self.stop.get_mut().cleanup_complete = false;
                    processing.open().await.map_err(ApplyError::Failed)?;
                }
            }
            if let Some(reads) = &self.output_reads {
                reads.begin_update();
            }
            update.prepared = true;
        }
        if update.result.is_none() {
            if update.operation.is_none() {
                let payload = wire::encode(&lifecycle.with_update(update.request.clone()))
                    .map_err(ApplyError::Damaged)?;
                let apply_fn = self
                    .management
                    .as_ref()
                    .and_then(|management| management.apply)
                    .expect("validated management");
                match self.inner.begin_with(
                    abi::operation::APPLY,
                    &payload,
                    |state, input, out| unsafe { apply_fn(state, input, out) },
                ) {
                    Ok(operation) => update.operation = Some(operation),
                    Err(error) => {
                        // The native side never accepted this apply.
                        update.result = Some(Err(Failure::failed(format!("{error:#}"))
                            .with_data(serde_json::json!({ "apply": "failed" }))));
                    }
                }
            }
            if let Some(operation) = update.operation.as_mut() {
                let result = operation.await;
                update.operation.take();
                update.result = Some(result.and_then(|bytes| {
                    if bytes.is_empty() {
                        Ok(())
                    } else {
                        Err(Failure::protocol("native update returned unexpected data"))
                    }
                }));
            }
        }
        if reopen_processing {
            if let Some(processing) = &mut self.processing {
                processing.close().await.map_err(ApplyError::Damaged)?;
            }
            self.stop.get_mut().cleanup_complete = true;
        }
        if let Err(failure) = update.result.as_ref().expect("finished native update") {
            update.completed = true;
            if apply_unchanged(failure) {
                if let Some(reads) = &self.output_reads {
                    reads.finish_update();
                }
            }
            return Err(apply_error(failure.clone()));
        }
        self.finish_managed_update().map_err(ApplyError::Damaged)
    }

    fn prepare_managed_update(
        &self,
        apply: &ApplyContext,
        context: &ComponentUpdateContext,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !context.operation_id.trim().is_empty()
                && context.plan.mode != ComponentUpdateMode::Recreate
                && UpdateBoundary::from(context.plan.mode) == apply.boundary()
                && self
                    .metadata
                    .capabilities
                    .update_modes
                    .contains(&context.plan.mode),
            "native instance cannot apply the requested update mode"
        );
        anyhow::ensure!(
            self.management.is_some(),
            "native component does not export management"
        );
        anyhow::ensure!(
            !self.inner.retiring.load(Ordering::Acquire)
                && !self.inner.stopping.load(Ordering::Acquire),
            "native update requires completed lifecycle work"
        );
        anyhow::ensure!(
            context.proposed.component_id == *self.descriptor.id(),
            "native retained-object update cannot change identity, ports or resource bindings"
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
        Ok(())
    }

    fn finish_managed_update(&mut self) -> anyhow::Result<()> {
        let update = self.update.get_mut().as_mut().expect("owned update");
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

#[async_trait]
impl ComponentManagement for NativeComponentProxy {
    fn assess(
        &self,
        current: &ComponentSpecification,
        proposed: &ComponentSpecification,
    ) -> anyhow::Result<Option<Assessment>> {
        self.assess_managed_update(current, proposed)
    }

    async fn apply(
        &mut self,
        context: &ApplyContext,
        update: ComponentUpdateContext,
    ) -> std::result::Result<(), ApplyError> {
        self.apply_managed_update(context, update).await
    }
}
