// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Stateful transformers.
//!
//! A transformer reads changes on its input port, maintains state and
//! publishes the changes to that state on its output port. A Continuous Query
//! is one such transformer; so is any other component whose output is the
//! current state it maintains.
//!
//! This module is everything those transformers share:
//!
//! - lifecycle and the atomic, non-atomic and volatile storage profiles;
//! - configuration identity, producer epoch and committed input progress;
//! - bootstrap from a provider or from connected inputs, including a first
//!   input connected after the transformer started;
//! - restart replay of unconfirmed output, rebuild after a history gap or a
//!   first input that a running dependent must follow, crash-safe
//!   installation, and refusal when retained state cannot be trusted;
//! - the materialized output and its snapshot, snapshot-and-follow and replay
//!   readers, delivery confirmation and scheduled work;
//! - durable input images and private tables for the logic's own state;
//! - the update rules for the shared options and output retention.
//!
//! An implementation supplies only [`TransformerLogic`]: how input changes
//! update its private state and which output changes they produce.

mod binding;
mod component;
mod engine;
mod non_atomic;
mod observations;
mod output;
mod rebuild;
mod recovery;
mod state;
mod update;
mod volatile;

use std::{num::NonZeroUsize, sync::Arc};

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_computation_contracts::{
    ChangeEnvelope, ComponentId, ComponentUpdatePlan, GraphProducerIdentity, InputBinding,
    InputEnvelope, OutputProgress, OutputReaders, PortId, QueryRecoveryPolicy as RecoveryPolicy,
    Record, SourceTransactionLimits, StreamId,
};

pub use drasi_computation_contracts::telemetry::QueryObservationPhase as Phase;

pub use binding::{BoundContext, ContextBinding, ContextGuard};
pub use component::StatefulTransformer;
pub use observations::{Observations, Run};
pub use output::{
    apply_operations, difference, logical_sequence, restore_row, row_key, row_mutations,
    OutputState, Rows, TransformerOutput,
};
pub use state::{
    apply_inputs, apply_inputs_where, get_value, get_values, set_value, HostState, InputChange,
    StateStore,
};
#[cfg(any(test, feature = "test-support"))]
pub use state::{MemoryState, MemoryTransaction};
pub use update::{update_plan, validate_reconstruction};

use crate::{
    wire::{ApplyUpdateRequest, CreateRequest, DescribeRequest, UpdateRequest},
    Capabilities, ComponentDescription, ConfigOption, ConfigType, NativeProcessingContext,
    OutputDescription, ProgressPersistence, ResourceDurability,
};

/// The input port every stateful transformer reads.
pub const INPUT_PORT: &str = "in";
/// The output port every stateful transformer publishes its state on.
pub const OUTPUT_PORT: &str = "out";

/// What a stateful transformer implementation supplies.
///
/// The runtime calls these hooks inside host processing operations, so state
/// changes commit atomically with the transformer's output and input progress.
#[async_trait]
pub trait TransformerLogic: Send + Sync + Sized + 'static {
    /// Private state opened over the component's bound host stores, such as
    /// an evaluator and its indexes. Use `()` when the output is the state.
    type State: Send + Sync + 'static;
    /// One input batch decoded for evaluation.
    type Prepared: Send + 'static;
    /// What evaluating a batch produced, before it is projected to output.
    type Evaluation: Send + 'static;

    /// Bind private stores to one processing operation. Dropping the
    /// returned guard unbinds them.
    fn bind(&self, _context: &Arc<NativeProcessingContext>) -> Result<Box<dyn Send>> {
        Ok(Box::new(()))
    }

    /// Open private state inside a bound operation, after any owned reset.
    async fn open(&self, context: &Arc<NativeProcessingContext>) -> Result<Arc<Self::State>>;

    /// Clear in-memory state that an owned storage reset does not reach.
    async fn reset(&self, _state: &Self::State) -> Result<()> {
        Ok(())
    }

    /// The input scope a batch evaluates in, when the logic distinguishes
    /// its inputs.
    fn scope(&self, _input: &InputEnvelope, _phase: Phase) -> Result<Option<ComponentId>> {
        Ok(None)
    }

    /// The host changed the bound inputs.
    fn bind_inputs(&self, _bindings: &[InputBinding]) -> Result<()> {
        Ok(())
    }

    /// Decode one batch. `source` names the source that committed input
    /// progress attributes a steady batch to.
    fn prepare(
        &self,
        input: &ChangeEnvelope,
        scope: Option<ComponentId>,
        source: Option<ComponentId>,
        phase: Phase,
    ) -> Result<Self::Prepared>;

    /// Apply a prepared batch to private state.
    async fn evaluate(
        &self,
        state: &Self::State,
        prepared: &mut Self::Prepared,
        phase: Phase,
        observations: &Observations,
    ) -> Result<Self::Evaluation>;

    /// The output changes an evaluation produced, as an envelope on the
    /// output stream numbered `sequence`, or `None` when nothing changed.
    fn project(
        &self,
        input: &ChangeEnvelope,
        prepared: Self::Prepared,
        evaluation: Self::Evaluation,
        sequence: u64,
    ) -> Result<Option<ChangeEnvelope>>;

    /// Annotate an output after its transaction committed.
    fn committed(&self, _output: &mut ChangeEnvelope, _core_return_ns: u64) -> Result<()> {
        Ok(())
    }

    /// Whether processing changed the logic's own schedule since last asked.
    fn take_schedule_change(&self) -> bool {
        false
    }

    /// The physical time the logic next has scheduled work.
    async fn next_due(&self, _state: &Self::State) -> Result<Option<u64>> {
        Ok(None)
    }

    /// Run due scheduled work, returning its output numbered `sequence`.
    async fn due(
        &self,
        _state: &Self::State,
        _sequence: u64,
        _observations: &Observations,
    ) -> Result<Option<ChangeEnvelope>> {
        Ok(None)
    }

    /// Add the logic's own annotations to an output before it is stored.
    fn decorate(
        &self,
        _output: &mut ChangeEnvelope,
        _producer: &GraphProducerIdentity,
        _generation: u64,
    ) -> Result<()> {
        Ok(())
    }

    /// Check a stored, retained or confirmed output against this logic.
    fn verify(
        &self,
        _output: &ChangeEnvelope,
        _producer: &GraphProducerIdentity,
        _generation: u64,
    ) -> Result<()> {
        Ok(())
    }

    /// Refuse a reset that would discard `output` when it belongs to another
    /// owner than `producer`'s transformer. Undecodable outputs are allowed.
    fn verify_owner(
        &self,
        _output: &ChangeEnvelope,
        _producer: &GraphProducerIdentity,
    ) -> Result<()> {
        Ok(())
    }

    /// Storage keys and current images of the rows `output` changed.
    fn row_mutations(
        &self,
        output: &ChangeEnvelope,
        after: &Rows,
    ) -> Result<Vec<(u64, Option<Bytes>)>> {
        Ok(output::row_mutations(output, after))
    }

    /// Rebuild a stored row.
    fn restore_row(&self, key: u64, payload: Bytes) -> Result<Record> {
        output::restore_row(key, payload)
    }

    /// The output that moves a dependent from `old` to `new` after a rebuild
    /// read `upstream` again.
    fn difference(
        &self,
        old: &Rows,
        new: &Rows,
        stream: &StreamId,
        sequence: u64,
        _upstream: &StreamId,
    ) -> Result<Option<ChangeEnvelope>> {
        output::difference(old, new, stream.clone(), sequence)
    }

    /// Whether re-reading the single input reproduces this state, so a
    /// rebuild can replace it.
    fn rebuild_supported(&self) -> bool {
        true
    }

    /// Readers of the output beyond snapshot, follow and replay. Called once
    /// when the transformer is created.
    fn readers(&self, _output: &Arc<TransformerOutput>, readers: OutputReaders) -> OutputReaders {
        readers
    }

    /// Plan an update. `None` when the logic does not accept updates.
    /// `state` is the open state, when the transformer is running, for a
    /// plan that depends on what it retains. [`update_plan`] applies the
    /// shared rules.
    fn plan_update(
        &self,
        _state: Option<&Self::State>,
        _request: &UpdateRequest,
    ) -> Option<Result<ComponentUpdatePlan>> {
        None
    }

    /// Apply an approved in-place update, returning the retained output
    /// capacity the new configuration requires.
    fn apply_update(&self, _request: &ApplyUpdateRequest) -> Result<NonZeroUsize> {
        anyhow::bail!("this transformer does not accept updates")
    }

    /// Whether the logic accepts updates. Must agree with the factory's
    /// declared management capability.
    fn manages_updates(&self) -> bool {
        false
    }
}

/// What the logic declares about its output when the transformer is created.
pub struct Settings {
    pub stream: StreamId,
    /// Retained output history, in outputs.
    pub capacity: NonZeroUsize,
    /// The snapshot dataset name.
    pub dataset: String,
    /// The logic configuration retained state was built from. Retained
    /// state is reused only with the same value.
    pub identity: serde_json::Value,
    /// Complete source transactions; these require atomic publication.
    pub source_transactions: Option<SourceTransactionLimits>,
}

/// The runtime's own configuration options, shared by every stateful
/// transformer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    pub bootstrap: bool,
    pub volatile: bool,
    pub non_atomic: bool,
    pub recovery: RecoveryPolicy,
    pub reset_configuration: bool,
}

impl Options {
    pub fn parse(configuration: &serde_json::Value) -> Result<Self> {
        let flag = |name: &str| -> Result<bool> {
            configuration
                .get(name)
                .map(|value| {
                    value
                        .as_bool()
                        .with_context(|| format!("{name} must be a boolean"))
                })
                .transpose()
                .map(Option::unwrap_or_default)
        };
        let volatile = flag("volatile")?;
        let non_atomic = match configuration.get("publication") {
            None => false,
            Some(serde_json::Value::String(value)) if value == "atomic" => false,
            Some(serde_json::Value::String(value)) if value == "non_atomic" => true,
            _ => anyhow::bail!("publication must be 'atomic' or 'non_atomic'"),
        };
        let recovery = match configuration.get("recovery") {
            None => RecoveryPolicy::Strict,
            Some(serde_json::Value::String(value)) if value == "strict" => RecoveryPolicy::Strict,
            Some(serde_json::Value::String(value)) if value == "auto_reset" => {
                RecoveryPolicy::AutoReset
            }
            _ => anyhow::bail!("recovery must be 'strict' or 'auto_reset'"),
        };
        Ok(Self {
            bootstrap: flag("bootstrap")?,
            volatile,
            non_atomic: non_atomic && !volatile,
            recovery,
            reset_configuration: flag("resetConfiguration")?,
        })
    }
}

/// The runtime's optional configuration fields, for a factory's schema.
pub fn configuration_fields() -> [(&'static str, ConfigType, bool); 6] {
    [
        ("bootstrap", ConfigType::Boolean, false),
        ("volatile", ConfigType::Boolean, false),
        ("publication", ConfigType::String, false),
        ("recovery", ConfigType::String, false),
        ("resetConfiguration", ConfigType::Boolean, false),
        ("outboxCapacity", ConfigType::Integer, false),
    ]
}

/// The runtime's optional configuration fields with their documentation.
pub fn configuration_options() -> [ConfigOption; 6] {
    [
        ConfigOption::new(
            "bootstrap",
            ConfigType::Boolean,
            false,
            "Initialize state from the bound bootstrap provider or the connected input's current state.",
        ),
        ConfigOption::new(
            "volatile",
            ConfigType::Boolean,
            false,
            "Keep state in memory only; every initialization publishes a new output epoch.",
        ),
        ConfigOption::new(
            "publication",
            ConfigType::String,
            false,
            "Publication of state, output and input progress: atomic (default) or non_atomic.",
        ),
        ConfigOption::new(
            "recovery",
            ConfigType::String,
            false,
            "Recovery policy for retained state that cannot be reused: strict (default) or auto_reset.",
        ),
        ConfigOption::new(
            "resetConfiguration",
            ConfigType::Boolean,
            false,
            "Reset retained state when the configuration changed.",
        ),
        ConfigOption::new(
            "outboxCapacity",
            ConfigType::Integer,
            false,
            "Committed outputs retained for replay and restart delivery (default 1).",
        ),
    ]
}

/// The capabilities every stateful transformer declares. Implementations add
/// their own, such as a semantic kind or extra readers.
pub fn capabilities(manages_updates: bool) -> Capabilities {
    let input = || PortId::try_new(INPUT_PORT).expect("constant port");
    let output = || PortId::try_new(OUTPUT_PORT).expect("constant port");
    Capabilities {
        output_progress: Some(OutputProgress::Origin),
        output_snapshots: vec![output()],
        output_follows: vec![output()],
        output_replays: vec![output()],
        continuations: true,
        startup_continuation: true,
        wakeups: true,
        input_binding_updates: true,
        management: manages_updates,
        indexed_processing: true,
        volatile_processing: true,
        non_atomic_publication: true,
        committed_input_progress: Some(input()),
        durable_bootstrap: Some(input()),
        durable_output: true,
        erases_owned_state: true,
        ..Capabilities::default()
    }
}

/// Outputs follow the durability the host's bound processing provider
/// declares; a configured bootstrap reads upstream state.
pub fn describe(request: &DescribeRequest) -> Result<ComponentDescription> {
    let durability = request
        .bound_durability("processing")
        .unwrap_or(ResourceDurability::Undeclared);
    let progress = match durability {
        ResourceDurability::Durable => ProgressPersistence::Durable,
        ResourceDurability::Volatile => ProgressPersistence::Volatile,
        ResourceDurability::Undeclared => ProgressPersistence::Undeclared,
    };
    Ok(ComponentDescription::new(progress)
        .with_upstream_initial_state(Options::parse(&request.request.configuration)?.bootstrap)
        .with_outputs(
            &request.descriptor,
            OutputDescription::following(durability),
        ))
}

/// The retained output history a configuration names, defaulting to one.
pub fn output_capacity(request: &CreateRequest) -> Result<NonZeroUsize> {
    Ok(request
        .configuration
        .get("outboxCapacity")
        .map(|value| serde_json::from_value::<NonZeroUsize>(value.clone()))
        .transpose()
        .context("invalid output retention capacity")?
        .unwrap_or(NonZeroUsize::MIN))
}

