// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_computation_contracts::{BinaryEnvelopeCodec, ChangeEnvelope, GraphProducerProgress};
use drasi_core::computation::{
    ComputationIndexProvider, ComputationIndexes, ComputationTransaction,
};
use drasi_core_types::{CheckpointKey, ComponentId, OutputPartition};
use drasi_lib::computation::v1::{
    OutputBindingError, OutputBindingState, OutputBindings, OutputReservations,
    OUTPUT_BINDINGS_CHECKPOINT,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

pub(super) const DELIVERED: &str = "\0computation:processing-delivered:v1";

pub(super) fn reserved(key: &str) -> bool {
    key == DELIVERED || key == OUTPUT_BINDINGS_CHECKPOINT
}

#[derive(Clone)]
struct Stored {
    bindings: OutputBindings,
    delivered: Option<u64>,
    head: Option<u64>,
}

impl Stored {
    fn pending(&self) -> bool {
        self.head.unwrap_or(0) > self.delivered.unwrap_or(0)
    }
}

pub(super) struct Output {
    pub(super) failure: Arc<AtomicBool>,
    pub(super) publication: Arc<OutputBindingState>,
    current: Mutex<Option<Stored>>,
    bound: AtomicBool,
    component: ComponentId,
    graph: String,
    codec: Arc<BinaryEnvelopeCodec>,
    atomic: bool,
}

impl Output {
    pub(super) fn new(
        component: ComponentId,
        graph: String,
        codec: Arc<BinaryEnvelopeCodec>,
        atomic: bool,
    ) -> Self {
        let failure = Arc::new(AtomicBool::new(false));
        Self {
            publication: OutputBindingState::new(failure.clone()),
            failure,
            current: Mutex::new(None),
            bound: AtomicBool::new(false),
            component,
            graph,
            codec,
            atomic,
        }
    }

    pub(super) fn bind_owner(
        &self,
        provider: &Arc<dyn ComputationIndexProvider>,
        owner: &Arc<ComputationTransaction>,
    ) -> Result<()> {
        if self.atomic {
            owner.resources().atomic_result_transaction()?;
        } else {
            anyhow::ensure!(
                !owner.is_transaction_group_member()
                    && owner.resources().outbox_writer().is_some()
                    && owner.resources().live_results_writer().is_some(),
                "non-atomic output requires standalone persistent output resources"
            );
        }
        self.publication.invalidate()?;
        *self
            .current
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)? = None;
        self.publication
            .bind_standalone_owner(provider, Arc::downgrade(owner))?;
        self.failure.store(false, Ordering::Release);
        self.bound.store(false, Ordering::Release);
        Ok(())
    }

    pub(super) fn bound(&self) {
        self.bound.store(true, Ordering::Release);
    }

    pub(super) fn ensure_bound(&self) -> Result<()> {
        let current = self
            .current
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)?;
        let state = current
            .as_ref()
            .ok_or(OutputBindingError::RecoveryRequired)?;
        anyhow::ensure!(
            state.bindings.destinations().is_empty() || self.bound.load(Ordering::Acquire),
            "retained output destinations must be rebound before processing or replay"
        );
        Ok(())
    }

    pub(super) fn validate_bindings(
        &self,
        owner: &ComputationTransaction,
        bindings: &OutputBindings,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.failure.load(Ordering::Acquire),
            OutputBindingError::RecoveryRequired
        );
        anyhow::ensure!(
            self.current
                .lock()
                .map_err(|_| OutputBindingError::Poisoned)?
                .is_some(),
            OutputBindingError::RecoveryRequired
        );
        self.publication.validate(bindings)?;
        anyhow::ensure!(
            self.atomic || !bindings.has_shared(),
            "non-atomic publication cannot bind shared transaction-group destinations"
        );
        bindings.validate_transaction(owner)?;
        Ok(())
    }

    pub(super) async fn reserve(&self) -> Result<Option<OutputReservations>> {
        self.ensure_bound()?;
        let bindings = self
            .current
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)?
            .as_ref()
            .context("output progress is not recovered")?
            .bindings
            .clone();
        if bindings.has_shared() {
            Ok(Some(Box::pin(bindings.reserve()).await?))
        } else {
            Ok(None)
        }
    }
}

pub(super) struct Draft {
    output: Arc<Output>,
    state: Mutex<Option<Stored>>,
    emission: Mutex<Option<ChangeEnvelope>>,
    emits: bool,
    baseline_allowed: AtomicBool,
}

impl Draft {
    pub(super) fn new(output: &Arc<Output>, emits: bool) -> Result<Self> {
        anyhow::ensure!(
            !output.failure.load(Ordering::Acquire),
            OutputBindingError::RecoveryRequired
        );
        Ok(Self {
            output: output.clone(),
            state: Mutex::new(None),
            emission: Mutex::new(None),
            emits,
            baseline_allowed: AtomicBool::new(false),
        })
    }

    async fn load(
        &self,
        resources: &ComputationIndexes,
        component: &ComponentId,
    ) -> Result<Stored> {
        if let Some(state) = self
            .state
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)?
            .clone()
        {
            return Ok(state);
        }
        let store = resources
            .checkpoint_store()
            .context("missing output checkpoint store")?;
        let delivered = store
            .read_checkpoint(&CheckpointKey::Metadata(DELIVERED.into()))
            .await?;
        let mut bindings = OutputBindings::load(
            store
                .read_checkpoint(&CheckpointKey::Metadata(OUTPUT_BINDINGS_CHECKPOINT.into()))
                .await?
                .as_ref(),
        )?;
        bindings.validate_progress(
            delivered
                .as_ref()
                .and_then(|value| value.source_position.as_ref()),
        )?;
        let head = store
            .read_result_sequence(&OutputPartition::Component(component.clone()))
            .await?;
        let delivered = delivered.map(|value| value.sequence);
        anyhow::ensure!(
            delivered.unwrap_or(0) <= head.unwrap_or(0)
                && head.unwrap_or(0) - delivered.unwrap_or(0) <= 1,
            "invalid native output delivery progress"
        );
        if let Some(current) = self
            .output
            .current
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)?
            .as_ref()
        {
            anyhow::ensure!(
                current.bindings == bindings,
                "stored output bindings changed outside the processing owner"
            );
            bindings = current.bindings.clone();
        }
        let state = Stored {
            bindings,
            delivered,
            head,
        };
        *self
            .state
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)? = Some(state.clone());
        Ok(state)
    }

    pub(super) async fn read(
        &self,
        resources: &ComputationIndexes,
        component: &ComponentId,
    ) -> Result<Option<u64>> {
        Ok(self.load(resources, component).await?.delivered)
    }

    pub(super) async fn stage_delivery(
        &self,
        resources: &ComputationIndexes,
        component: &ComponentId,
        sequence: u64,
    ) -> Result<()> {
        let mut state = self.load(resources, component).await?;
        anyhow::ensure!(
            sequence >= state.delivered.unwrap_or(0) && sequence <= state.head.unwrap_or(0),
            "delivery progress cannot regress or skip committed output"
        );
        state.delivered = Some(sequence);
        self.stage(resources, state).await
    }

    async fn stage(&self, resources: &ComputationIndexes, state: Stored) -> Result<()> {
        let store = resources
            .checkpoint_store()
            .context("missing output checkpoint store")?;
        store
            .stage_checkpoint(
                &CheckpointKey::Metadata(OUTPUT_BINDINGS_CHECKPOINT.into()),
                1,
                Some(&state.bindings.encode()?),
            )
            .await?;
        store
            .stage_checkpoint(
                &CheckpointKey::Metadata(DELIVERED.into()),
                state.delivered.context("missing delivered output")?,
                state.bindings.fingerprint(),
            )
            .await?;
        *self
            .state
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)? = Some(state);
        Ok(())
    }

    pub(super) async fn bind(
        &self,
        resources: &ComputationIndexes,
        component: &ComponentId,
        bindings: &OutputBindings,
    ) -> Result<()> {
        let mut state = self.load(resources, component).await?;
        state.bindings.validate_change(bindings, state.pending())?;
        state.bindings = bindings.clone();
        self.stage(resources, state).await
    }

    pub(super) async fn stage_head(
        &self,
        resources: &ComputationIndexes,
        component: &ComponentId,
        sequence: u64,
    ) -> Result<()> {
        let mut state = self.load(resources, component).await?;
        anyhow::ensure!(
            sequence >= state.head.unwrap_or(0)
                && sequence >= state.delivered.unwrap_or(0)
                && sequence - state.delivered.unwrap_or(0) <= 1,
            "native output cannot overwrite an unconfirmed result"
        );
        state.head = Some(sequence);
        *self
            .state
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)? = Some(state);
        Ok(())
    }

    pub(super) async fn validate_append(
        &self,
        resources: &ComputationIndexes,
        component: &ComponentId,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.load(resources, component).await?.pending(),
            "native output cannot trim an unconfirmed result"
        );
        Ok(())
    }

    pub(super) fn prepare_emission(&self, sequence: u64, data: &[u8]) -> Result<ChangeEnvelope> {
        anyhow::ensure!(
            self.emits,
            "retained output requires an output processing operation"
        );
        anyhow::ensure!(
            self.emission
                .lock()
                .map_err(|_| OutputBindingError::Poisoned)?
                .is_none(),
            "an output processing operation may retain only one output"
        );
        let envelope = self.output.codec.decode(data)?;
        let progress = GraphProducerProgress::from_envelope(&envelope)?
            .context("retained output lacks producer progress")?;
        anyhow::ensure!(
            progress.identity().component_id() == &self.output.component
                && progress.identity().graph_id() == self.output.graph
                && progress.identity().stream() == envelope.system().stream()
                && progress.identity().persistent()
                && progress.sequence() == sequence,
            "retained output does not belong to this actual processing scope"
        );
        Ok(envelope)
    }

    pub(super) fn record_emission(&self, envelope: ChangeEnvelope) -> Result<()> {
        let mut emission = self
            .emission
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)?;
        anyhow::ensure!(
            emission.is_none(),
            "output processing emitted more than one result"
        );
        *emission = Some(envelope);
        Ok(())
    }

    pub(super) fn emission(&self) -> Result<Option<ChangeEnvelope>> {
        Ok(self
            .emission
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)?
            .clone())
    }

    pub(super) async fn validate_reset(
        &self,
        resources: &ComputationIndexes,
        component: &ComponentId,
        preserve: bool,
    ) -> Result<()> {
        let state = self.load(resources, component).await?;
        anyhow::ensure!(
            state.bindings.destinations().is_empty(),
            OutputBindingError::ResetRequiresUnboundDestinations
        );
        anyhow::ensure!(
            !preserve || !state.pending(),
            "bootstrap reset cannot discard unconfirmed output"
        );
        Ok(())
    }

    pub(super) fn reset(&self, preserve_bootstrap: bool) -> Result<()> {
        *self
            .state
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)? = Some(Stored {
            bindings: OutputBindings::default(),
            delivered: None,
            head: Some(0),
        });
        self.baseline_allowed
            .store(preserve_bootstrap, Ordering::Release);
        Ok(())
    }

    pub(super) async fn initialize_reset_output(
        &self,
        resources: &ComputationIndexes,
        component: &ComponentId,
        sequence: u64,
    ) -> Result<()> {
        anyhow::ensure!(
            self.baseline_allowed.swap(false, Ordering::AcqRel),
            "output baseline requires a bootstrap reset in this operation"
        );
        let mut state = self.load(resources, component).await?;
        anyhow::ensure!(
            state.head == Some(0)
                && state.delivered.is_none()
                && state.bindings.destinations().is_empty(),
            "output baseline cannot overwrite initialized output or destinations"
        );
        state.head = Some(sequence);
        state.delivered = Some(sequence);
        self.stage(resources, state).await?;
        resources
            .checkpoint_store()
            .context("missing reset checkpoint store")?
            .stage_result_sequence(&OutputPartition::Component(component.clone()), sequence)
            .await?;
        Ok(())
    }

    pub(super) fn publish(&self) -> Result<()> {
        if let Some(state) = self
            .state
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)?
            .take()
        {
            if state.delivered.is_some() {
                self.output
                    .publication
                    .publish(state.bindings.clone(), state.pending())?;
            } else {
                self.output.publication.invalidate()?;
            }
            *self
                .output
                .current
                .lock()
                .map_err(|_| OutputBindingError::Poisoned)? = Some(state);
        }
        Ok(())
    }
}
