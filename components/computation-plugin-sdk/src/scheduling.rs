// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Scheduled work uses the existing processing transaction, not a second owner.

use std::sync::Arc;

use async_trait::async_trait;
use drasi_computation_contracts::{
    ScheduledRequest, ScheduledWork, ScheduledWorkToken, WakeupSource,
};
use drasi_core_types::interface::IndexError;

use crate::{
    abi::processing::{request, schedule_observation},
    processing::{NativeProcessing, NativeProcessingContext},
};

/// Read-only, revocable observation of a component's committed scheduled work.
///
/// Request, replace, cancel and completion are available only on the borrowed
/// [`NativeProcessingContext`]. Waiting starts no plugin-owned scheduler task.
#[derive(Clone)]
pub struct NativeScheduling(pub(crate) NativeProcessing);

impl NativeScheduling {
    /// The earliest committed deadline, in Unix epoch milliseconds.
    pub async fn next_deadline(&self) -> anyhow::Result<Option<u64>> {
        self.0
            .observe_schedule(schedule_observation::NEXT_DEADLINE)
            .await
    }

    /// Return this observer for the transformer's normal graph-owned wakeup.
    pub fn wakeup_source(&self) -> Arc<dyn WakeupSource> {
        Arc::new(self.clone())
    }
}

#[async_trait]
impl WakeupSource for NativeScheduling {
    async fn wait(&self) -> anyhow::Result<()> {
        self.0.observe_schedule(schedule_observation::WAIT).await
    }

    async fn has_pending(&self) -> anyhow::Result<bool> {
        Ok(self.next_deadline().await?.is_some())
    }
}

impl NativeProcessingContext {
    /// Request previously absent work alongside component state. Neither the
    /// request nor its deadline is committed until the enclosing run succeeds.
    pub async fn request_scheduled_work(
        &self,
        request: ScheduledRequest,
    ) -> Result<ScheduledWork, IndexError> {
        self.call(request::SCHEDULE_REQUEST, &request).await
    }

    /// Replace exactly the named incarnation with a new deadline and payload.
    /// Stale tokens and changed keys are errors, never implicit new requests.
    pub async fn replace_scheduled_work(
        &self,
        expected: &ScheduledWorkToken,
        request: ScheduledRequest,
    ) -> Result<ScheduledWork, IndexError> {
        self.call(request::SCHEDULE_REPLACE, &(expected, request))
            .await
    }

    /// Cancel exactly the named pending incarnation in this transaction.
    pub async fn cancel_scheduled_work(
        &self,
        expected: &ScheduledWorkToken,
    ) -> Result<(), IndexError> {
        self.call(request::SCHEDULE_CANCEL, expected).await
    }

    /// Read a request from the current transaction's scheduling draft.
    pub async fn scheduled_work(&self, key: &str) -> Result<Option<ScheduledWork>, IndexError> {
        self.call(request::SCHEDULE_GET, &key).await
    }

    /// Claim one eligible request in this transaction. A successful operation
    /// must complete its claim; error, cancellation or rollback retains the
    /// request. Only one claim may be outstanding within an operation.
    pub async fn claim_due_scheduled_work(&self) -> Result<Option<ScheduledWork>, IndexError> {
        self.call(request::SCHEDULE_CLAIM_DUE, &()).await
    }

    /// Complete the exact current claim alongside business state and retained
    /// output. This is not acknowledgement of an external effect or delivery.
    pub async fn complete_scheduled_work(
        &self,
        token: &ScheduledWorkToken,
    ) -> Result<(), IndexError> {
        self.call(request::SCHEDULE_COMPLETE, token).await
    }
}
