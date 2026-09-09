// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use serde_json::json;

use crate::activation::{
    next_action, Action, Activation, ActivationState, MessageId, Operation, TerminalUpdatePolicy,
};
use crate::client::{
    build_cancel_task_rpc, build_send_message_rpc, parse_send_message_result, CancelTaskRequest,
    OutboundPart, SendMessageRequest, SendMessageResult,
};
use crate::descriptor::{A2AReactionConfigDto, RecoveryPolicyDto};
use crate::process::{extract_result_key, DiffPayload};
use crate::{A2AReaction, A2AReactionBuilder};
use drasi_lib::Reaction;

#[test]
fn next_action_table_cells_are_covered() {
    let active = ActivationState::Present(Activation::ActiveTask {
        task_id: "task-1".to_string(),
        context_id: "ctx-1".to_string(),
        state: "WORKING".to_string(),
        sequence: 1,
    });
    let one_shot = ActivationState::Present(Activation::OneShot {
        message_id: MessageId("msg-1".to_string()),
        sequence: 1,
    });
    let terminal = ActivationState::Present(Activation::TerminalTask {
        task_id: "task-9".to_string(),
        context_id: "ctx-9".to_string(),
        state: "COMPLETED".to_string(),
        sequence: 1,
    });

    assert!(matches!(
        next_action(
            Operation::Add,
            &ActivationState::Absent,
            TerminalUpdatePolicy::Replace
        ),
        Action::SendCreate
    ));
    assert!(matches!(
        next_action(Operation::Add, &active, TerminalUpdatePolicy::Replace),
        Action::SendFollowUp { .. }
    ));
    assert!(matches!(
        next_action(Operation::Add, &one_shot, TerminalUpdatePolicy::Replace),
        Action::Drop { .. }
    ));
    assert!(matches!(
        next_action(Operation::Add, &terminal, TerminalUpdatePolicy::Replace),
        Action::SendCreate
    ));
    assert!(matches!(
        next_action(Operation::Add, &terminal, TerminalUpdatePolicy::Ignore),
        Action::Drop { .. }
    ));

    assert!(matches!(
        next_action(
            Operation::Update,
            &ActivationState::Absent,
            TerminalUpdatePolicy::Replace
        ),
        Action::SendCreate
    ));
    assert!(matches!(
        next_action(Operation::Update, &active, TerminalUpdatePolicy::Replace),
        Action::SendFollowUp { .. }
    ));
    assert!(matches!(
        next_action(Operation::Update, &one_shot, TerminalUpdatePolicy::Replace),
        Action::Drop { .. }
    ));
    assert!(matches!(
        next_action(Operation::Update, &terminal, TerminalUpdatePolicy::Replace),
        Action::SendCreate
    ));
    assert!(matches!(
        next_action(Operation::Update, &terminal, TerminalUpdatePolicy::Ignore),
        Action::Drop { .. }
    ));

    assert!(matches!(
        next_action(Operation::Delete, &active, TerminalUpdatePolicy::Replace),
        Action::Cancel { .. }
    ));
    assert!(matches!(
        next_action(
            Operation::Delete,
            &ActivationState::Absent,
            TerminalUpdatePolicy::Replace
        ),
        Action::Drop { .. }
    ));
    assert!(matches!(
        next_action(Operation::Delete, &one_shot, TerminalUpdatePolicy::Replace),
        Action::Drop { .. }
    ));
    assert!(matches!(
        next_action(Operation::Delete, &terminal, TerminalUpdatePolicy::Replace),
        Action::Drop { .. }
    ));
}

#[test]
fn result_key_extraction_reports_missing_field() {
    let payload = DiffPayload {
        operation: Operation::Add,
        before: None,
        after: Some(json!({"invoiceId":"INV-1"})),
        data: None,
    };
    let result = extract_result_key(&payload, &["invoiceId".to_string(), "tenantId".to_string()]);
    assert!(result.is_err());
}

#[test]
fn send_message_parser_distinguishes_task_and_message() {
    let task = parse_send_message_result(&json!({
        "id": "task-1",
        "contextId": "ctx-1",
        "status": { "state": "WORKING" }
    }))
    .expect("task parse");
    assert!(matches!(
        task,
        SendMessageResult::Task {
            terminal: false,
            ..
        }
    ));

    let wrapped = parse_send_message_result(&json!({
        "task": {
            "id": "task-9f2",
            "contextId": "ctx-4417",
            "status": { "state": "TASK_STATE_WORKING" }
        }
    }))
    .expect("wrapped task parse");
    assert_eq!(
        wrapped,
        SendMessageResult::Task {
            task_id: "task-9f2".to_string(),
            context_id: "ctx-4417".to_string(),
            state: "WORKING".to_string(),
            terminal: false,
        }
    );

    let terminal = parse_send_message_result(&json!({
        "task": {
            "id": "task-9",
            "contextId": "ctx-9",
            "status": { "state": "completed" }
        }
    }))
    .expect("terminal task parse");
    assert!(matches!(
        terminal,
        SendMessageResult::Task { terminal: true, .. }
    ));

    let message = parse_send_message_result(&json!({
        "message": {
            "role": "ROLE_AGENT",
            "parts":[{"text":"done"}]
        }
    }))
    .expect("message parse");
    assert_eq!(message, SendMessageResult::Message);

    let invalid = parse_send_message_result(&json!({"id":"not-a-task"}));
    assert!(invalid.is_err());
}

#[test]
fn cancel_task_rpc_shape_matches_contract() {
    let rpc = build_cancel_task_rpc(&CancelTaskRequest {
        message_id: "msg-1".to_string(),
        task_id: "task-1".to_string(),
    });
    assert_eq!(rpc["jsonrpc"], json!("2.0"));
    assert_eq!(rpc["method"], json!("CancelTask"));
    assert_eq!(rpc["params"]["id"], json!("task-1"));
}

#[test]
fn send_message_rpc_shape_sets_role_and_parts() {
    let rpc = build_send_message_rpc(&SendMessageRequest {
        message_id: "msg-1".to_string(),
        activation_id: "query:key:1".to_string(),
        task_id: None,
        parts: vec![
            OutboundPart::Text("Investigate".to_string()),
            OutboundPart::Data {
                media_type: "application/vnd.drasi.change+json".to_string(),
                data: json!({"queryId":"q1","operation":"ADD"}),
            },
        ],
        return_immediately: true,
    });
    assert_eq!(rpc["method"], json!("SendMessage"));
    assert_eq!(rpc["id"], json!("msg-1"));
    assert_eq!(rpc["params"]["message"]["role"], json!("ROLE_USER"));
    assert_eq!(rpc["params"]["message"]["messageId"], json!("msg-1"));
    assert!(rpc["params"]["message"].get("taskId").is_none());
    assert_eq!(
        rpc["params"]["configuration"]["returnImmediately"],
        json!(true)
    );
    assert_eq!(
        rpc["params"]["message"]["metadata"]["activationId"],
        json!("query:key:1")
    );
    let parts = &rpc["params"]["message"]["parts"];
    assert!(parts[0].get("kind").is_none());
    assert_eq!(parts[0]["text"], json!("Investigate"));
    assert_eq!(
        parts[1]["mediaType"],
        json!("application/vnd.drasi.change+json")
    );
    assert_eq!(parts[1]["data"]["queryId"], json!("q1"));
}

#[test]
fn terminal_policy_replace_vs_ignore() {
    let terminal = ActivationState::Present(Activation::TerminalTask {
        task_id: "task-9".to_string(),
        context_id: "ctx-9".to_string(),
        state: "COMPLETED".to_string(),
        sequence: 1,
    });

    assert!(matches!(
        next_action(Operation::Update, &terminal, TerminalUpdatePolicy::Replace),
        Action::SendCreate
    ));
    assert!(matches!(
        next_action(Operation::Update, &terminal, TerminalUpdatePolicy::Ignore),
        Action::Drop { .. }
    ));
}

#[test]
fn builder_enforces_result_key_fields_and_durable_hooks() {
    let missing = A2AReactionBuilder::new("a2a")
        .with_query("q1")
        .with_endpoint("http://localhost:8080")
        .build();
    assert!(missing.is_err());

    let reaction: A2AReaction = A2AReaction::builder("a2a")
        .with_query("q1")
        .with_endpoint("http://localhost:8080")
        .with_result_key_fields(["invoiceId"])
        .build()
        .expect("reaction build");
    assert!(reaction.is_durable());
    assert!(!reaction.needs_snapshot_on_fresh_start());
}

#[test]
fn descriptor_recovery_policy_allows_strict_and_auto_skip_gap_only() {
    let strict: A2AReactionConfigDto = serde_json::from_value(json!({
        "endpoint": "http://localhost:8080",
        "resultKeyFields": ["invoiceId"],
        "recoveryPolicy": "strict"
    }))
    .expect("strict parse");
    assert_eq!(strict.recovery_policy, Some(RecoveryPolicyDto::Strict));

    let skip: A2AReactionConfigDto = serde_json::from_value(json!({
        "endpoint": "http://localhost:8080",
        "resultKeyFields": ["invoiceId"],
        "recoveryPolicy": "auto_skip_gap"
    }))
    .expect("auto_skip_gap parse");
    assert_eq!(skip.recovery_policy, Some(RecoveryPolicyDto::AutoSkipGap));

    let reset = serde_json::from_value::<A2AReactionConfigDto>(json!({
        "endpoint": "http://localhost:8080",
        "resultKeyFields": ["invoiceId"],
        "recoveryPolicy": "auto_reset"
    }));
    assert!(reset.is_err());
}
