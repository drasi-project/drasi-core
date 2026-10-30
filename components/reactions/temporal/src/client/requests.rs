// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{ensure, Result};
use temporalio_common::protos::temporal::api::{
    common::v1::WorkflowType,
    enums::v1::{
        UpdateWorkflowExecutionLifecycleStage, WorkflowIdConflictPolicy, WorkflowIdReusePolicy,
    },
    taskqueue::v1::TaskQueue,
    update::v1 as update,
    workflowservice::v1::*,
};

use super::{execution, payload};
use crate::{config::*, render::Command};

pub(super) enum ActionRequest {
    Signal(SignalWorkflowExecutionRequest),
    Start(StartWorkflowExecutionRequest),
    Update(UpdateWorkflowExecutionRequest),
    Cancel(RequestCancelWorkflowExecutionRequest),
    Terminate(TerminateWorkflowExecutionRequest),
}

pub(super) fn build(config: &TemporalReactionConfig, command: &Command) -> Result<ActionRequest> {
    let namespace = config.namespace.clone();
    let identity = "drasi-temporal-reaction".to_owned();
    Ok(match &command.action {
        Action::Signal { signal_name, .. } => {
            ActionRequest::Signal(SignalWorkflowExecutionRequest {
                namespace,
                workflow_execution: Some(execution(command)),
                signal_name: signal_name.clone(),
                input: payload(command)?,
                identity,
                request_id: command.request_id.clone(),
                ..Default::default()
            })
        }
        Action::Start {
            workflow_id,
            workflow_type,
            task_queue,
            conflict_policy,
            reuse_policy,
        } => ActionRequest::Start(StartWorkflowExecutionRequest {
            namespace,
            workflow_id: workflow_id.clone(),
            workflow_type: Some(WorkflowType {
                name: workflow_type.clone(),
            }),
            task_queue: Some(TaskQueue {
                name: task_queue.clone(),
                ..Default::default()
            }),
            input: payload(command)?,
            identity,
            request_id: command.request_id.clone(),
            workflow_id_conflict_policy: match conflict_policy {
                ConflictPolicy::Fail => WorkflowIdConflictPolicy::Fail as i32,
                ConflictPolicy::UseExisting => WorkflowIdConflictPolicy::UseExisting as i32,
            },
            workflow_id_reuse_policy: match reuse_policy {
                ReusePolicy::RejectDuplicate => WorkflowIdReusePolicy::RejectDuplicate as i32,
                ReusePolicy::AllowDuplicate => WorkflowIdReusePolicy::AllowDuplicate as i32,
                ReusePolicy::AllowDuplicateFailedOnly => {
                    WorkflowIdReusePolicy::AllowDuplicateFailedOnly as i32
                }
            },
            ..Default::default()
        }),
        Action::Update {
            update_name,
            wait_for,
            ..
        } => ActionRequest::Update(UpdateWorkflowExecutionRequest {
            namespace,
            workflow_execution: Some(execution(command)),
            wait_policy: Some(update::WaitPolicy {
                lifecycle_stage: match wait_for {
                    UpdateWait::Accepted => UpdateWorkflowExecutionLifecycleStage::Accepted,
                    UpdateWait::Completed => UpdateWorkflowExecutionLifecycleStage::Completed,
                } as i32,
            }),
            request: Some(update::Request {
                request_id: command.request_id.clone(),
                meta: Some(update::Meta {
                    update_id: command.request_id.clone(),
                    identity,
                }),
                input: Some(update::Input {
                    name: update_name.clone(),
                    args: payload(command)?,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        Action::Cancel { reason, .. } => {
            ensure!(
                !execution(command).run_id.is_empty(),
                "cancel requires a persisted runId"
            );
            ActionRequest::Cancel(RequestCancelWorkflowExecutionRequest {
                namespace,
                workflow_execution: Some(execution(command)),
                reason: reason.clone(),
                identity,
                request_id: command.request_id.clone(),
                ..Default::default()
            })
        }
        Action::Terminate { reason, .. } => {
            ensure!(
                !execution(command).run_id.is_empty(),
                "terminate requires a persisted runId"
            );
            ActionRequest::Terminate(TerminateWorkflowExecutionRequest {
                namespace,
                workflow_execution: Some(execution(command)),
                reason: reason.clone(),
                identity,
                details: payload(command)?,
                ..Default::default()
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use temporalio_common::protos::temporal::api::common::v1::Payloads;

    fn command(action: Value, value: Option<Value>) -> Command {
        Command {
            request_id: "stable-request".into(),
            action: serde_json::from_value(action).unwrap(),
            payload: value,
            resolved_run_id: None,
        }
    }

    fn assert_payload(payload: Option<Payloads>, expected: &Option<Value>) {
        match expected {
            None => assert!(payload.is_none()),
            Some(value) => {
                let payload = payload.unwrap();
                assert_eq!(payload.payloads.len(), 1);
                assert_eq!(payload.payloads[0].metadata["encoding"], b"json/plain");
                assert_eq!(
                    serde_json::from_slice::<Value>(&payload.payloads[0].data).unwrap(),
                    *value
                );
            }
        }
    }

    #[test]
    fn signal_and_update_preserve_identity_target_and_payload() {
        let config = TemporalReactionConfig {
            namespace: "test-namespace".into(),
            ..Default::default()
        };
        for value in [None, Some(json!({"value":42}))] {
            for run in [None, Some("explicit-run")] {
                let signal = command(
                    json!({
                        "action":"signal", "workflowId":"target", "runId":run, "signalName":"notify"
                    }),
                    value.clone(),
                );
                let ActionRequest::Signal(request) = build(&config, &signal).unwrap() else {
                    panic!("wrong action")
                };
                assert_eq!(request.namespace, "test-namespace");
                assert_eq!(request.request_id, signal.request_id);
                assert_eq!(request.signal_name, "notify");
                let target = request.workflow_execution.unwrap();
                assert_eq!(target.workflow_id, "target");
                assert_eq!(target.run_id, run.unwrap_or_default());
                assert_payload(request.input, &value);
                for (wait, stage) in [
                    ("accepted", UpdateWorkflowExecutionLifecycleStage::Accepted),
                    (
                        "completed",
                        UpdateWorkflowExecutionLifecycleStage::Completed,
                    ),
                ] {
                    let update = command(
                        json!({
                            "action":"update", "workflowId":"target", "runId":run, "updateName":"change", "waitFor":wait
                        }),
                        value.clone(),
                    );
                    let ActionRequest::Update(request) = build(&config, &update).unwrap() else {
                        panic!("wrong action")
                    };
                    assert_eq!(request.namespace, "test-namespace");
                    assert_eq!(request.workflow_execution.unwrap(), target);
                    assert_eq!(request.wait_policy.unwrap().lifecycle_stage, stage as i32);
                    let request = request.request.unwrap();
                    assert_eq!(request.request_id, update.request_id);
                    assert_eq!(request.meta.unwrap().update_id, update.request_id);
                    let input = request.input.unwrap();
                    assert_eq!(input.name, "change");
                    assert_payload(input.args, &value);
                }
            }
        }
    }

    #[test]
    fn start_maps_all_conflict_and_reuse_policies() {
        for (conflict, expected_conflict) in [
            ("fail", WorkflowIdConflictPolicy::Fail),
            ("use_existing", WorkflowIdConflictPolicy::UseExisting),
        ] {
            for (reuse, expected_reuse) in [
                ("reject_duplicate", WorkflowIdReusePolicy::RejectDuplicate),
                ("allow_duplicate", WorkflowIdReusePolicy::AllowDuplicate),
                (
                    "allow_duplicate_failed_only",
                    WorkflowIdReusePolicy::AllowDuplicateFailedOnly,
                ),
            ] {
                for value in [None, Some(json!({"start":true}))] {
                    let command = command(
                        json!({
                            "action":"start", "workflowId":"target", "workflowType":"TestWorkflow", "taskQueue":"test",
                            "conflictPolicy":conflict, "reusePolicy":reuse
                        }),
                        value.clone(),
                    );
                    let config = TemporalReactionConfig::default();
                    let ActionRequest::Start(request) = build(&config, &command).unwrap() else {
                        panic!("wrong action")
                    };
                    assert_eq!(request.namespace, config.namespace);
                    assert_eq!(request.workflow_id, "target");
                    assert_eq!(request.workflow_type.unwrap().name, "TestWorkflow");
                    assert_eq!(request.task_queue.unwrap().name, "test");
                    assert_eq!(request.request_id, command.request_id);
                    assert_eq!(
                        request.workflow_id_conflict_policy,
                        expected_conflict as i32
                    );
                    assert_eq!(request.workflow_id_reuse_policy, expected_reuse as i32);
                    assert_payload(request.input, &value);
                }
            }
        }
    }

    #[test]
    fn destructive_requests_require_a_run_and_prefer_the_persisted_pin() {
        for action in ["cancel", "terminate"] {
            for value in [None, Some(json!({"detail":true}))] {
                let mut command = command(
                    json!({
                        "action":action, "workflowId":"target", "reason":"test reason"
                    }),
                    value.clone(),
                );
                let config = TemporalReactionConfig::default();
                assert!(build(&config, &command).is_err());
                match &mut command.action {
                    Action::Cancel { run_id, .. } | Action::Terminate { run_id, .. } => {
                        *run_id = Some("explicit-run".into())
                    }
                    _ => unreachable!(),
                }
                for pin in [None, Some("persisted-run")] {
                    command.resolved_run_id = pin.map(str::to_owned);
                    let (namespace, target, reason) = match build(&config, &command).unwrap() {
                        ActionRequest::Cancel(request) => {
                            assert_eq!(request.request_id, command.request_id);
                            (
                                request.namespace,
                                request.workflow_execution.unwrap(),
                                request.reason,
                            )
                        }
                        ActionRequest::Terminate(request) => {
                            assert_payload(request.details, &value);
                            (
                                request.namespace,
                                request.workflow_execution.unwrap(),
                                request.reason,
                            )
                        }
                        _ => panic!("wrong action"),
                    };
                    assert_eq!(namespace, config.namespace);
                    assert_eq!(target.workflow_id, "target");
                    assert_eq!(target.run_id, pin.unwrap_or("explicit-run"));
                    assert_eq!(reason, "test reason");
                }
            }
        }
    }
}
