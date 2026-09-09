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

use std::collections::HashMap;

use anyhow::Context;
use handlebars::Handlebars;
use log::{debug, error, info, warn};

use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::reactions::common::base::ReactionBase;
use drasi_lib::reactions::common::{CheckpointState, FailureAction};
use drasi_lib::state_store::StateStoreError;
use drasi_lib::ReactionRecoveryPolicy;

use crate::activation::{
    next_action, Action, Activation, ActivationState, MessageId, Operation, ResultKey,
};
use crate::client::{
    A2AClient, CancelTaskRequest, DeliveryResult, OutboundPart, SendMessageRequest,
    SendMessageResult,
};
use crate::config::A2AReactionConfig;

const ACTIVATION_STATE_KEY_PREFIX: &str = "activation";
const DATA_MEDIA_TYPE: &str = "application/vnd.drasi.change+json";

pub fn build_handlebars() -> Handlebars<'static> {
    let mut handlebars = Handlebars::new();
    handlebars.register_helper(
        "json",
        Box::new(
            |h: &handlebars::Helper,
             _: &Handlebars,
             _: &handlebars::Context,
             _: &mut handlebars::RenderContext,
             out: &mut dyn handlebars::Output|
             -> handlebars::HelperResult {
                if let Some(value) = h.param(0) {
                    let rendered =
                        serde_json::to_string(value.value()).unwrap_or_else(|_| "null".to_string());
                    out.write(&rendered)?;
                }
                Ok(())
            },
        ),
    );
    handlebars
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_loop(
    reaction_name: String,
    base: ReactionBase,
    config: A2AReactionConfig,
    client: A2AClient,
    handlebars: Handlebars<'static>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    mut checkpoints: CheckpointState,
    policy: ReactionRecoveryPolicy,
) {
    let status_handle = base.status_handle();
    let mut activation_cache: HashMap<String, ActivationState> = HashMap::new();

    loop {
        let query_result = tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                break;
            }
            query_result = base.priority_queue.dequeue() => query_result,
        };

        let query_result = query_result.as_ref();
        if query_result.results.is_empty() {
            continue;
        }

        let mut delivery_failed = false;
        for diff in &query_result.results {
            if matches!(diff, ResultDiff::Noop) {
                continue;
            }
            let delivery = process_diff(
                &reaction_name,
                &base,
                &config,
                &client,
                &handlebars,
                query_result,
                diff,
                &mut activation_cache,
            )
            .await;

            if let Err(error) = delivery {
                delivery_failed = true;
                error!(
                    "[{reaction_name}] Failed processing query '{}' seq {}: {error:#}",
                    query_result.query_id, query_result.sequence
                );
                if FailureAction::from_policy(policy) == FailureAction::Stop {
                    break;
                }
            }
        }

        if delivery_failed {
            match FailureAction::from_policy(policy) {
                FailureAction::Stop => {
                    status_handle
                        .set_status(
                            ComponentStatus::Error,
                            Some(format!(
                                "A2A delivery failed for query '{}' (seq {}); stopped per recovery policy",
                                query_result.query_id, query_result.sequence
                            )),
                        )
                        .await;
                    return;
                }
                FailureAction::SkipAndContinue => {
                    warn!(
                        "[{reaction_name}] Delivery failed for query '{}' seq {}, skipping per AutoSkipGap",
                        query_result.query_id, query_result.sequence
                    );
                    if let Err(error) = checkpoints
                        .advance(&base, &query_result.query_id, query_result.sequence)
                        .await
                    {
                        error!(
                            "[{reaction_name}] Failed advancing checkpoint after skip for query '{}' seq {}: {error:#}",
                            query_result.query_id, query_result.sequence
                        );
                    }
                    continue;
                }
            }
        }

        if let Err(error) = checkpoints
            .advance(&base, &query_result.query_id, query_result.sequence)
            .await
        {
            error!(
                "[{reaction_name}] Failed writing checkpoint for query '{}' seq {}: {error:#}",
                query_result.query_id, query_result.sequence
            );
            if FailureAction::from_policy(policy) == FailureAction::Stop {
                status_handle
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!(
                            "A2A checkpoint write failed for query '{}' (seq {}); stopped per recovery policy",
                            query_result.query_id, query_result.sequence
                        )),
                    )
                    .await;
                return;
            }
        }
    }

    info!("[{reaction_name}] A2A processing loop stopped");
    status_handle
        .set_status(
            ComponentStatus::Stopped,
            Some("A2A reaction processing task stopped".to_string()),
        )
        .await;
}

#[allow(clippy::too_many_arguments)]
async fn process_diff(
    reaction_name: &str,
    base: &ReactionBase,
    config: &A2AReactionConfig,
    client: &A2AClient,
    handlebars: &Handlebars<'static>,
    query_result: &QueryResult,
    diff: &ResultDiff,
    activation_cache: &mut HashMap<String, ActivationState>,
) -> anyhow::Result<()> {
    let Some(diff_payload) = DiffPayload::from_result_diff(diff) else {
        return Ok(());
    };

    let result_key = match extract_result_key(&diff_payload, &config.result_key_fields) {
        Ok(result_key) => result_key,
        Err(error) => {
            warn!(
                "[{reaction_name}] Dropping '{}' diff at seq {} due to missing result key: {error:#}",
                diff_payload.operation.as_str(),
                query_result.sequence
            );
            return Ok(());
        }
    };

    let state_key = activation_state_key(&query_result.query_id, &result_key);
    let activation = load_activation_state(base, activation_cache, &state_key)
        .await
        .with_context(|| {
            format!("failed loading activation state key '{state_key}' from state store")
        })?;

    let action = next_action(
        diff_payload.operation,
        &activation,
        config.terminal_update_policy,
    );
    let message_id = message_id(
        reaction_name,
        &query_result.query_id,
        &result_key,
        query_result.sequence,
    );
    let activation_id = activation_id(&query_result.query_id, &result_key, query_result.sequence);
    let parts = build_parts(
        config,
        handlebars,
        query_result,
        &diff_payload,
        result_key.as_str(),
    )?;

    match action {
        Action::SendCreate => {
            let outcome = client
                .send_message(SendMessageRequest {
                    message_id: message_id.to_string(),
                    activation_id,
                    task_id: None,
                    parts,
                    return_immediately: config.return_immediately,
                })
                .await
                .context("SendMessage create RPC failed")?;
            match outcome {
                DeliveryResult::Delivered(response) => {
                    let next_state = response_to_activation(response, message_id, query_result);
                    save_activation_state(base, activation_cache, &state_key, &next_state)
                        .await
                        .with_context(|| {
                            format!(
                                "failed saving activation state key '{state_key}' after SendCreate"
                            )
                        })?;
                }
                DeliveryResult::Dropped => {
                    debug!(
                        "[{reaction_name}] SendCreate dropped for query '{}' result key '{}'",
                        query_result.query_id, result_key
                    );
                }
            }
        }
        Action::SendFollowUp { task_id } => {
            let outcome = client
                .send_message(SendMessageRequest {
                    message_id: message_id.to_string(),
                    activation_id,
                    task_id: Some(task_id),
                    parts,
                    return_immediately: config.return_immediately,
                })
                .await
                .context("SendMessage follow-up RPC failed")?;
            match outcome {
                DeliveryResult::Delivered(response) => {
                    let next_state = match response {
                        SendMessageResult::Message => activation.clone(),
                        response => response_to_activation(response, message_id, query_result),
                    };
                    save_activation_state(base, activation_cache, &state_key, &next_state)
                        .await
                        .with_context(|| {
                            format!(
                                "failed saving activation state key '{state_key}' after SendFollowUp"
                            )
                        })?;
                }
                DeliveryResult::Dropped => {
                    debug!(
                        "[{reaction_name}] SendFollowUp dropped for query '{}' result key '{}'",
                        query_result.query_id, result_key
                    );
                }
            }
        }
        Action::Cancel { task_id } => {
            let outcome = client
                .cancel_task(CancelTaskRequest {
                    message_id: message_id.to_string(),
                    task_id,
                })
                .await
                .context("CancelTask RPC failed")?;
            if matches!(
                outcome,
                DeliveryResult::Delivered(()) | DeliveryResult::Dropped
            ) {
                clear_activation_state(base, activation_cache, &state_key)
                    .await
                    .with_context(|| {
                        format!("failed clearing activation state key '{state_key}' after cancel")
                    })?;
            }
        }
        Action::Drop { reason } => {
            debug!(
                "[{reaction_name}] Dropping '{}' diff for query '{}' result key '{}': {reason}",
                diff_payload.operation.as_str(),
                query_result.query_id,
                result_key
            );
            if diff_payload.operation == Operation::Delete
                && matches!(activation, ActivationState::Present(_))
            {
                clear_activation_state(base, activation_cache, &state_key)
                    .await
                    .with_context(|| {
                        format!(
                            "failed clearing activation state key '{state_key}' after DELETE drop"
                        )
                    })?;
            }
        }
    }

    Ok(())
}

fn response_to_activation(
    response: SendMessageResult,
    message_id: MessageId,
    query_result: &QueryResult,
) -> ActivationState {
    match response {
        SendMessageResult::Task {
            task_id,
            context_id,
            state,
            terminal,
        } => {
            if terminal {
                ActivationState::Present(Activation::TerminalTask {
                    task_id,
                    context_id,
                    state,
                    sequence: query_result.sequence,
                })
            } else {
                ActivationState::Present(Activation::ActiveTask {
                    task_id,
                    context_id,
                    state,
                    sequence: query_result.sequence,
                })
            }
        }
        SendMessageResult::Message => ActivationState::Present(Activation::OneShot {
            message_id,
            sequence: query_result.sequence,
        }),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DiffPayload {
    pub(crate) operation: Operation,
    pub(crate) before: Option<serde_json::Value>,
    pub(crate) after: Option<serde_json::Value>,
    pub(crate) data: Option<serde_json::Value>,
}

impl DiffPayload {
    fn from_result_diff(diff: &ResultDiff) -> Option<Self> {
        match diff {
            ResultDiff::Add { data, .. } => Some(Self {
                operation: Operation::Add,
                before: None,
                after: Some(data.clone()),
                data: None,
            }),
            ResultDiff::Delete { data, .. } => Some(Self {
                operation: Operation::Delete,
                before: Some(data.clone()),
                after: None,
                data: None,
            }),
            ResultDiff::Update {
                data,
                before,
                after,
                ..
            } => Some(Self {
                operation: Operation::Update,
                before: Some(before.clone()),
                after: Some(after.clone()),
                data: Some(data.clone()),
            }),
            ResultDiff::Aggregation { before, after, .. } => Some(Self {
                operation: Operation::Update,
                before: before.clone(),
                after: Some(after.clone()),
                data: None,
            }),
            ResultDiff::Noop => None,
        }
    }
}

pub(crate) fn extract_result_key(
    payload: &DiffPayload,
    result_key_fields: &[String],
) -> anyhow::Result<ResultKey> {
    let source = match payload.operation {
        Operation::Delete => payload.before.as_ref(),
        Operation::Add | Operation::Update => payload.after.as_ref(),
    }
    .ok_or_else(|| anyhow::anyhow!("result payload did not include source object"))?;

    if result_key_fields.len() == 1 {
        let field = &result_key_fields[0];
        let value = lookup_field(source, field)
            .ok_or_else(|| anyhow::anyhow!("missing required result key field '{field}'"))?;
        return Ok(ResultKey(value_to_key_fragment(value)));
    }

    let mut parts = Vec::with_capacity(result_key_fields.len());
    for field in result_key_fields {
        let value = lookup_field(source, field)
            .ok_or_else(|| anyhow::anyhow!("missing required result key field '{field}'"))?;
        let encoded_field = urlencoding::encode(field);
        let value_fragment = value_to_key_fragment(value);
        let encoded_value = urlencoding::encode(&value_fragment);
        parts.push(format!("{encoded_field}={encoded_value}"));
    }
    Ok(ResultKey(parts.join("&")))
}

fn lookup_field<'a>(root: &'a serde_json::Value, field: &str) -> Option<&'a serde_json::Value> {
    let mut value = root;
    for segment in field.split('.') {
        value = value.get(segment)?;
    }
    Some(value)
}

fn value_to_key_fragment(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(v) => v.to_string(),
        serde_json::Value::Number(v) => v.to_string(),
        serde_json::Value::String(v) => v.clone(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => value.to_string(),
    }
}

fn build_parts(
    config: &A2AReactionConfig,
    handlebars: &Handlebars<'static>,
    query_result: &QueryResult,
    payload: &DiffPayload,
    result_key: &str,
) -> anyhow::Result<Vec<OutboundPart>> {
    let mut parts = Vec::new();
    if let Some(template) = &config.instruction_template {
        let instruction_context = instruction_context(query_result, payload, result_key);
        let rendered = handlebars
            .render_template(template, &instruction_context)
            .context("failed rendering instructionTemplate")?;
        if !rendered.trim().is_empty() {
            parts.push(OutboundPart::Text(rendered));
        }
    }
    parts.push(OutboundPart::Data {
        media_type: DATA_MEDIA_TYPE.to_string(),
        data: data_payload(query_result, payload, result_key),
    });
    Ok(parts)
}

fn instruction_context(
    query_result: &QueryResult,
    payload: &DiffPayload,
    result_key: &str,
) -> serde_json::Value {
    serde_json::json!({
        "query_id": query_result.query_id,
        "operation": payload.operation.as_str(),
        "sequence": query_result.sequence,
        "result_key": result_key,
        "before": payload.before,
        "after": payload.after,
        "metadata": query_result.metadata,
    })
}

fn data_payload(
    query_result: &QueryResult,
    payload: &DiffPayload,
    result_key: &str,
) -> serde_json::Value {
    let mut root = serde_json::Map::new();
    root.insert(
        "queryId".to_string(),
        serde_json::Value::String(query_result.query_id.clone()),
    );
    root.insert(
        "operation".to_string(),
        serde_json::Value::String(payload.operation.as_str().to_string()),
    );
    root.insert(
        "sequence".to_string(),
        serde_json::Value::Number(query_result.sequence.into()),
    );
    root.insert(
        "resultKey".to_string(),
        serde_json::Value::String(result_key.to_string()),
    );
    if let Some(before) = &payload.before {
        root.insert("before".to_string(), before.clone());
    }
    if let Some(after) = &payload.after {
        root.insert("after".to_string(), after.clone());
    }
    if let Some(data) = &payload.data {
        root.insert("data".to_string(), data.clone());
    }
    if !query_result.metadata.is_empty() {
        root.insert(
            "metadata".to_string(),
            serde_json::Value::Object(query_result.metadata.clone().into_iter().collect()),
        );
    }
    serde_json::Value::Object(root)
}

fn message_id(
    reaction_id: &str,
    query_id: &str,
    result_key: &ResultKey,
    sequence: u64,
) -> MessageId {
    MessageId(format!("{reaction_id}-{query_id}-{result_key}-{sequence}"))
}

fn activation_id(query_id: &str, result_key: &ResultKey, sequence: u64) -> String {
    format!("{query_id}:{result_key}:{sequence}")
}

fn activation_state_key(query_id: &str, result_key: &ResultKey) -> String {
    format!("{ACTIVATION_STATE_KEY_PREFIX}:{query_id}:{result_key}")
}

async fn load_activation_state(
    base: &ReactionBase,
    activation_cache: &mut HashMap<String, ActivationState>,
    state_key: &str,
) -> anyhow::Result<ActivationState> {
    if let Some(state) = activation_cache.get(state_key) {
        return Ok(state.clone());
    }

    let Some(store) = base.state_store().await else {
        activation_cache.insert(state_key.to_string(), ActivationState::Absent);
        return Ok(ActivationState::Absent);
    };

    let value = store
        .get(&base.id, state_key)
        .await
        .map_err(state_store_error)
        .with_context(|| format!("state store read failed for key '{state_key}'"))?;

    let state = match value {
        Some(bytes) => {
            let activation: Activation = serde_json::from_slice(&bytes).with_context(|| {
                format!("state store activation decode failed for key '{state_key}'")
            })?;
            ActivationState::Present(activation)
        }
        None => ActivationState::Absent,
    };
    activation_cache.insert(state_key.to_string(), state.clone());
    Ok(state)
}

async fn save_activation_state(
    base: &ReactionBase,
    activation_cache: &mut HashMap<String, ActivationState>,
    state_key: &str,
    state: &ActivationState,
) -> anyhow::Result<()> {
    activation_cache.insert(state_key.to_string(), state.clone());
    if let ActivationState::Absent = state {
        clear_activation_state(base, activation_cache, state_key).await?;
        return Ok(());
    }

    let Some(store) = base.state_store().await else {
        return Ok(());
    };
    let ActivationState::Present(activation) = state else {
        return Ok(());
    };
    let bytes = serde_json::to_vec(activation).context("failed encoding activation state")?;
    store
        .set(&base.id, state_key, bytes)
        .await
        .map_err(state_store_error)
        .with_context(|| format!("state store write failed for key '{state_key}'"))?;
    Ok(())
}

async fn clear_activation_state(
    base: &ReactionBase,
    activation_cache: &mut HashMap<String, ActivationState>,
    state_key: &str,
) -> anyhow::Result<()> {
    activation_cache.remove(state_key);
    let Some(store) = base.state_store().await else {
        return Ok(());
    };
    store
        .delete(&base.id, state_key)
        .await
        .map_err(state_store_error)
        .with_context(|| format!("state store delete failed for key '{state_key}'"))?;
    Ok(())
}

fn state_store_error(error: StateStoreError) -> anyhow::Error {
    anyhow::anyhow!("state store error: {error}")
}
