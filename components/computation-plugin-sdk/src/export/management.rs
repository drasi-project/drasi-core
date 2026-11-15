// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The management extension table: instance assessment and update application.
//! Apply shares the component busy slot, state machine, fencing and quiescence
//! with lifecycle operations; it differs only in its entry point.

use super::*;
use crate::lifecycle::{ApplyContext, ApplyError, UpdateBoundary};

pub(super) static MANAGEMENT_V1: abi::management::PluginManagementV1 =
    abi::management::PluginManagementV1 {
        header: Header::new::<abi::management::PluginManagementV1>(),
        version: abi::management::VERSION,
        reserved: 0,
        assess: Some(assess),
        apply: Some(apply),
    };

/// Synchronous and side-effect free; BUSY while another operation holds the
/// component.
unsafe extern "C" fn assess(state: *mut c_void, input: BorrowedBytes) -> Reply {
    transport::reply_boundary(|| {
        let instance = unsafe { state_ref::<Arc<Instance>>(state)? }.clone();
        if instance.damaged.is_damaged() {
            return Err(Failure::panicked());
        }
        if !instance.factory.capabilities.management {
            return Err(Failure::new(
                abi::status::UNSUPPORTED,
                "native component does not export management",
            ));
        }
        let request = unsafe { update::decode_update_request(input)? };
        instance
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| busy())?;
        let _guard = BusyGuard(instance.clone());
        let mut component = instance.component.try_lock().map_err(|_| busy())?;
        let assessment =
            update::assess_instance(&instance, &mut component, &request).map_err(Failure::from)?;
        update::encode_assessment(&assessment)
    })
}

fn busy() -> Failure {
    Failure::new(
        abi::status::BUSY,
        "native data/lifecycle operation is already active",
    )
}

unsafe extern "C" fn apply(
    state: *mut c_void,
    input: BorrowedBytes,
    out: *mut abi::OperationHandle,
) -> Status {
    transport::status_boundary(|| {
        require_output(out)?;
        let instance = unsafe { state_ref::<Arc<Instance>>(state)? }.clone();
        let input = unsafe { copy_input(input)? };
        let operation = start_operation(instance, abi::operation::APPLY, input, None)?;
        unsafe { transport::write_out(out, operation) }
    })
}

/// Executes an accepted APPLY at its Pause or Stop boundary.
pub(super) async fn execute_apply(instance: Arc<Instance>, call: Call) -> anyhow::Result<Vec<u8>> {
    quiesce(&instance).await?;
    let mut component = instance.component.lock().await;
    let (update, attempted, configuration) = {
        let state = instance.administration.state()?;
        let pending = state
            .update
            .as_ref()
            .context("missing accepted native update")?;
        (
            pending.request.clone(),
            pending.attempted,
            state.configuration.clone(),
        )
    };
    anyhow::ensure!(
        !attempted,
        "a previous native apply did not complete; stop and dispose the component"
    );
    anyhow::ensure!(
        component.base().configuration()? == configuration,
        "native component configuration changed outside the approved update"
    );
    if let Err(error) = update::recheck_apply(&instance, &mut component, &update) {
        instance.administration.state()?.update = None;
        return Err(apply_failure(error).into());
    }
    instance
        .administration
        .state()?
        .update
        .as_mut()
        .expect("accepted update")
        .attempted = true;
    let scope = call.scope();
    let context = ApplyContext::new(scope.clone(), UpdateBoundary::from(update.plan.mode));
    let management = component
        .base_mut()
        .management()
        .context("native component stopped exporting management")?;
    let watch = deadline_watch(&scope);
    let applied = management.apply(&context, &update).await;
    if let Some(watch) = watch {
        watch.abort();
    }
    if let Err(error) = applied {
        // A damaged object keeps the attempted update pending, so only STOP and
        // DISPOSE remain admissible; the host then retires it.
        if !error.damages_object() {
            instance.administration.state()?.update = None;
        }
        return Err(apply_failure(error).into());
    }
    // The object changed; a mismatch here leaves the update pending, so only
    // STOP and DISPOSE remain admissible.
    instance
        .factory
        .validate_instance(component.base().descriptor(), &update.request.proposed.id)?;
    let configuration = component.base().configuration()?;
    instance.factory.configuration.validate(&configuration)?;
    let matches = match factory_configuration_details(&instance.factory_state) {
        Some(details) => details.equivalent_with_defaults(
            &instance.factory.configuration,
            &configuration,
            &update.request.proposed.configuration,
        )?,
        None => configuration == update.request.proposed.configuration,
    };
    anyhow::ensure!(
        matches,
        "native apply did not expose the approved proposed configuration"
    );
    let queries = output_reads::updated_queries(
        &instance.factory,
        component.base().descriptor(),
        instance.output_reads.as_deref(),
        component.base().output_readers(),
    )?;
    let mut state = instance.administration.state()?;
    if let Some(reads) = &instance.output_reads {
        reads.publish_queries(queries)?;
    }
    state.request = update.request.proposed.clone();
    state.configuration = configuration;
    state.update = None;
    state.applied = Some(update);
    Ok(Vec::new())
}

fn apply_failure(error: ApplyError) -> Failure {
    let message = format!("{error:#}");
    match error {
        ApplyError::Refused { now_requires, .. } => Failure::new(abi::status::FAILED, message)
            .with_data(serde_json::json!({
                "apply": "refused",
                "nowRequires": now_requires,
            })),
        ApplyError::Failed(_) => {
            Failure::failed(message).with_data(serde_json::json!({ "apply": "failed" }))
        }
        ApplyError::Damaged(_) => {
            Failure::failed(message).with_data(serde_json::json!({ "apply": "damaged" }))
        }
    }
}
