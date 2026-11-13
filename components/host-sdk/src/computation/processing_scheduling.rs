// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Scheduling metadata and Core queue mutations in the existing processing owner.

use drasi_core_types::CheckpointKey;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::c_void,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use drasi_computation_contracts::{
    validate_scheduled_key, CommittedSchedule, PhysicalClock, ScheduledRequest, ScheduledWork,
    ScheduledWorkToken, MAX_SCHEDULED_REQUESTS, MAX_SCHEDULED_STATE_BYTES,
};
use drasi_computation_plugin_abi::{self as abi, processing::*};
use drasi_computation_plugin_sdk::{
    transport::{self, Failure},
    wire,
};
use drasi_core::{
    computation::{ComputationIndexes, ComputationTransaction},
    interface::{FailureMode, PushType},
    models::ElementReference,
};
use serde::{Deserialize, Serialize};

use super::State;

const CHECKPOINT: &str = "\0computation:scheduled-work:v1";
const QUEUE_SOURCE: &str = "\0computation:scheduled-work:v1";
const QUEUE_POSITION: usize = 0;

pub(super) fn reserved(key: &str) -> bool {
    key == CHECKPOINT
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    sequence: u64,
    work: ScheduledWork,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Records {
    sequence: u64,
    pending: BTreeMap<String, Pending>,
}

impl Records {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.pending.len() <= MAX_SCHEDULED_REQUESTS,
            "scheduled request count exceeds its bound"
        );
        let mut sequences = BTreeSet::new();
        let mut identities = BTreeSet::new();
        for (key, pending) in &self.pending {
            pending.work.validate()?;
            anyhow::ensure!(
                key == &pending.work.token.key
                    && pending.sequence > 0
                    && pending.sequence <= self.sequence
                    && sequences.insert(pending.sequence)
                    && identities.insert(pending.work.token.request_id),
                "invalid or duplicate scheduled request identity"
            );
        }
        Ok(())
    }

    fn next_due(&self) -> Option<u64> {
        self.pending
            .values()
            .map(|pending| pending.work.due_time_ms)
            .min()
    }

    fn get(&self, token: &ScheduledWorkToken) -> anyhow::Result<&Pending> {
        token.validate()?;
        let pending = self
            .pending
            .get(&token.key)
            .context("scheduled request is absent")?;
        anyhow::ensure!(
            pending.work.token == *token,
            "scheduled request was replaced"
        );
        Ok(pending)
    }
}

async fn load(resources: &ComputationIndexes) -> anyhow::Result<Records> {
    let checkpoint = resources
        .checkpoint_store()
        .context("scheduled processing requires checkpoints")?
        .read_checkpoint(&CheckpointKey::Metadata(CHECKPOINT.into()))
        .await?;
    let records = match checkpoint {
        Some(checkpoint) => {
            anyhow::ensure!(
                checkpoint.sequence == 1,
                "unsupported scheduling checkpoint version"
            );
            let bytes = checkpoint
                .source_position
                .context("missing scheduling checkpoint data")?;
            anyhow::ensure!(
                bytes.len() <= MAX_SCHEDULED_STATE_BYTES,
                "scheduling checkpoint exceeds its byte bound"
            );
            let records: Records = wire::decode(&bytes)?;
            records.validate()?;
            records
        }
        None => Records::default(),
    };
    anyhow::ensure!(
        resources.indexes().future_queue.peek_due_time().await? == records.next_due(),
        "scheduled request metadata does not match the committed future queue"
    );
    Ok(records)
}

pub(super) struct Scheduling {
    schedule: CommittedSchedule,
}

impl Scheduling {
    pub(super) fn new(clock: Option<Arc<dyn PhysicalClock>>) -> Self {
        Self {
            schedule: clock.map(CommittedSchedule::with_clock).unwrap_or_default(),
        }
    }

    pub(super) fn validate(&self, owner: &ComputationTransaction) -> anyhow::Result<()> {
        let resources = owner.resources();
        resources.atomic_result_transaction()?;
        resources
            .durability()
            .require(FailureMode::ProcessRestart)?;
        anyhow::ensure!(
            resources
                .checkpoint_store()
                .is_some_and(|store| store.is_persistent()),
            "scheduled processing requires persistent atomic checkpoints"
        );
        Ok(())
    }

    pub(super) async fn recover(&self, owner: &ComputationTransaction) -> anyhow::Result<()> {
        self.validate(owner)?;
        let deadline = owner
            .run(async {
                load(owner.resources())
                    .await
                    .map(|records| records.next_due())
                    .map_err(super::operation_error)
            })
            .await?;
        self.schedule.publish(deadline)
    }

    pub(super) fn suspend(&self) -> anyhow::Result<()> {
        self.schedule.publish(None)
    }

    pub(super) fn physical_time(&self) -> anyhow::Result<u64> {
        self.schedule.physical_time()
    }
}

struct Changes {
    records: Records,
    claimed: Option<ScheduledWorkToken>,
    dirty: bool,
}

/// One operation-local draft. It never owns a transaction or storage handle.
pub(super) struct Draft {
    scheduling: Arc<Scheduling>,
    changes: tokio::sync::Mutex<Option<Changes>>,
    publication: Mutex<Option<Option<u64>>>,
}

impl Draft {
    pub(super) fn new(scheduling: Arc<Scheduling>) -> Self {
        Self {
            scheduling,
            changes: tokio::sync::Mutex::new(None),
            publication: Mutex::new(None),
        }
    }

    pub(super) fn physical_time(&self) -> anyhow::Result<u64> {
        self.scheduling.physical_time()
    }

    pub(super) async fn apply(
        &self,
        resources: &ComputationIndexes,
        code: u32,
        input: &[u8],
    ) -> anyhow::Result<Vec<u8>> {
        let mut changes = self.changes.lock().await;
        if changes.is_none() {
            *changes = Some(Changes {
                records: load(resources).await?,
                claimed: None,
                dirty: false,
            });
        }
        let changes = changes.as_mut().context("missing scheduling draft")?;
        let queue = &resources.indexes().future_queue;
        match code {
            request::SCHEDULE_REQUEST | request::SCHEDULE_REPLACE => {
                let (expected, request): (Option<ScheduledWorkToken>, ScheduledRequest) =
                    if code == request::SCHEDULE_REPLACE {
                        let (token, request) = wire::decode(input)?;
                        (Some(token), request)
                    } else {
                        (None, wire::decode(input)?)
                    };
                request.validate()?;
                let previous = match &expected {
                    Some(token) => {
                        anyhow::ensure!(
                            token.key == request.key,
                            "replacement changes scheduled key"
                        );
                        anyhow::ensure!(
                            changes.claimed.as_ref() != Some(token),
                            "cannot replace claimed scheduled work"
                        );
                        Some(changes.records.get(token)?.sequence)
                    }
                    None => {
                        anyhow::ensure!(
                            !changes.records.pending.contains_key(&request.key),
                            "scheduled request already exists"
                        );
                        anyhow::ensure!(
                            changes.records.pending.len() < MAX_SCHEDULED_REQUESTS,
                            "scheduled request capacity exceeded"
                        );
                        None
                    }
                };
                let sequence = changes
                    .records
                    .sequence
                    .checked_add(1)
                    .context("scheduled request sequence exhausted")?;
                let work = ScheduledWork::try_new(request)?;
                if let Some(previous) = previous {
                    queue.remove(QUEUE_POSITION, previous).await?;
                }
                anyhow::ensure!(
                    queue
                        .push(
                            PushType::IfNotExists,
                            QUEUE_POSITION,
                            sequence,
                            &ElementReference::new(
                                QUEUE_SOURCE,
                                &work.token.request_id.to_string()
                            ),
                            0,
                            work.due_time_ms,
                        )
                        .await?,
                    "scheduled queue identity is already occupied"
                );
                changes.records.sequence = sequence;
                changes.records.pending.insert(
                    work.token.key.clone(),
                    Pending {
                        sequence,
                        work: work.clone(),
                    },
                );
                changes.dirty = true;
                wire::encode(&work)
            }
            request::SCHEDULE_CANCEL => {
                let token: ScheduledWorkToken = wire::decode(input)?;
                anyhow::ensure!(
                    changes.claimed.as_ref() != Some(&token),
                    "cannot cancel claimed scheduled work"
                );
                let sequence = changes.records.get(&token)?.sequence;
                queue.remove(QUEUE_POSITION, sequence).await?;
                changes.records.pending.remove(&token.key);
                changes.dirty = true;
                wire::encode(&())
            }
            request::SCHEDULE_GET => {
                let key: String = wire::decode(input)?;
                validate_scheduled_key(&key)?;
                wire::encode(
                    &changes
                        .records
                        .pending
                        .get(&key)
                        .map(|pending| &pending.work),
                )
            }
            request::SCHEDULE_CLAIM_DUE => {
                wire::decode::<()>(input)?;
                anyhow::ensure!(
                    changes.claimed.is_none(),
                    "complete the current scheduled claim first"
                );
                let now = self.physical_time()?;
                if changes.records.next_due().is_none_or(|due| due > now) {
                    return wire::encode(&Option::<ScheduledWork>::None);
                }
                let future = queue
                    .pop()
                    .await?
                    .context("scheduled queue lost pending work")?;
                let pending = changes
                    .records
                    .pending
                    .values()
                    .find(|pending| pending.sequence == future.group_signature)
                    .context("scheduled queue contains foreign work")?;
                anyhow::ensure!(
                    future.original_time == 0
                        && future.due_time == pending.work.due_time_ms
                        && future.due_time <= now
                        && future.element_ref
                            == ElementReference::new(
                                QUEUE_SOURCE,
                                &pending.work.token.request_id.to_string(),
                            ),
                    "scheduled queue request does not match its persisted identity"
                );
                let work = pending.work.clone();
                changes.claimed = Some(work.token.clone());
                wire::encode(&Some(work))
            }
            request::SCHEDULE_COMPLETE => {
                let token: ScheduledWorkToken = wire::decode(input)?;
                token.validate()?;
                anyhow::ensure!(
                    changes.claimed.as_ref() == Some(&token),
                    "scheduled completion does not match this transaction's claim"
                );
                changes.records.get(&token)?;
                changes.records.pending.remove(&token.key);
                changes.claimed = None;
                changes.dirty = true;
                wire::encode(&())
            }
            _ => anyhow::bail!("unsupported scheduled processing request"),
        }
    }

    pub(super) async fn stage(&self, resources: &ComputationIndexes) -> anyhow::Result<()> {
        let mut changes = self.changes.lock().await;
        if changes.is_none() {
            *changes = Some(Changes {
                records: load(resources).await?,
                claimed: None,
                dirty: false,
            });
        }
        let changes = changes.as_ref().context("missing scheduling draft")?;
        anyhow::ensure!(
            changes.claimed.is_none(),
            "successful processing must complete its scheduled claim"
        );
        if changes.dirty {
            changes.records.validate()?;
            let bytes = bytes::Bytes::from(wire::encode(&changes.records)?);
            anyhow::ensure!(
                bytes.len() <= MAX_SCHEDULED_STATE_BYTES,
                "scheduling checkpoint exceeds its byte bound"
            );
            resources
                .checkpoint_store()
                .context("missing scheduling checkpoint store")?
                .stage_checkpoint(&CheckpointKey::Metadata(CHECKPOINT.into()), 1, Some(&bytes))
                .await?;
        }
        *self
            .publication
            .lock()
            .map_err(|_| anyhow::anyhow!("scheduling draft poisoned"))? =
            Some(changes.records.next_due());
        Ok(())
    }

    pub(super) fn publish(&self) -> anyhow::Result<()> {
        let deadline = self
            .publication
            .lock()
            .map_err(|_| anyhow::anyhow!("scheduling draft poisoned"))?
            .context("scheduling draft was not staged")?;
        self.scheduling.schedule.publish(deadline)
    }
}

pub(super) unsafe extern "C" fn observe(
    context: *mut c_void,
    code: u32,
    out: *mut abi::OperationHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        if context.is_null() || out.is_null() {
            return Err(Failure::protocol(
                "null scheduled observation context or output",
            ));
        }
        if !matches!(
            code,
            schedule_observation::WAIT | schedule_observation::NEXT_DEADLINE
        ) {
            return Err(Failure::protocol("unknown scheduled observation request"));
        }
        let state = unsafe { &*context.cast::<State>() };
        let scheduling = state
            .scheduling
            .clone()
            .ok_or_else(|| Failure::protocol("scheduled processing was not negotiated"))?;
        let cancel = {
            let live = state
                .live
                .lock()
                .map_err(|_| Failure::failed("processing state poisoned"))?;
            if !live.active
                || live
                    .owner
                    .as_ref()
                    .is_none_or(|owner| owner.recovery_required())
            {
                return Err(Failure::closed());
            }
            live.cancel.clone()
        };
        let operation = transport::export_operation(
            async move {
                let work = async {
                    match code {
                        schedule_observation::WAIT => {
                            scheduling.schedule.wakeup_source().wait().await?;
                            wire::encode(&())
                        }
                        schedule_observation::NEXT_DEADLINE => {
                            wire::encode(&scheduling.schedule.next_due()?)
                        }
                        _ => unreachable!("validated observation"),
                    }
                };
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(Failure::closed()),
                    result = work => result.map_err(Failure::from),
                }
            },
            None,
        );
        unsafe { transport::write_out(out, operation) }
    })
}

#[cfg(test)]
#[path = "processing_scheduling_tests.rs"]
mod tests;
