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

use async_trait::async_trait;

use super::{ChangeEnvelope, ComponentDescriptor, PortId};

pub use drasi_computation_contracts::{
    InputEnvelope, OutputEnvelope, SinkBatching, SinkCompletion, SnapshotConsumer,
    SourceSchemaReader, WakeupSource,
};

/// Host-owned component lifecycle, independent of legacy Source/Reaction contexts.
///
/// The DAG host owns `Box<dyn ...>` instances, validates roles/ports before
/// starting, serializes all mutable calls, and owns cancellation and error policy.
/// Call processing methods only after successful start; drain/cancel processing
/// before stop. Dropping a cancelled future does not imply rollback of component
/// state or external effects. These traits do not promise restart, recovery, or
/// transactional lifecycle; see [`super::ComputationGraph`] for host policy.
///
/// The descriptor must remain unchanged throughout the owned component's life.
/// No default success methods conceal missing implementation.
#[async_trait]
pub trait ComputationComponent: Send + Sync {
    fn descriptor(&self) -> &ComponentDescriptor;
    /// Captured at construction, activation and reconfiguration, never per envelope.
    /// Unknown is not a recovery guarantee. Implementations must identify their
    /// actual admission, processing and publication boundary.
    fn recovery_contract(&self) -> super::ComponentRecovery {
        super::ComponentRecovery::default()
    }
    /// Publish read-only query capabilities at construction/reconfiguration
    /// boundaries. Non-query components do not expose a query API.
    fn query_api(&self) -> Option<std::sync::Arc<super::QueryApi>> {
        None
    }
    /// Publish optional source-data schema inspection independently of mutable
    /// processing. The graph captures this handle at construction/reconfiguration;
    /// the getter must not perform discovery or other network I/O.
    fn source_schema_reader(&self) -> Option<std::sync::Arc<dyn SourceSchemaReader>> {
        None
    }
    /// Optional snapshot/replay providers registered per declared output port.
    /// Capability is exactly the set of registered providers: a component that
    /// cannot actually serve a read registers nothing. Captured by the graph at
    /// construction/reconfiguration boundaries, never per envelope; the getter
    /// must not perform I/O. Providers must stay bound to this component
    /// generation's own state and return typed errors when they cannot answer.
    fn output_readers(&self) -> super::OutputReaders {
        super::OutputReaders::default()
    }
    /// Configuration values, not live processing state. Identity, lifecycle
    /// policy and external resource bindings are recorded by the graph.
    /// Values may change only through construction or reconfiguration, not
    /// through processing, so the host can publish them at those boundaries.
    ///
    /// Persistence may contain secrets; this is not a redacted inspection API.
    /// An opaque externally supplied component must report that configuration
    /// is unavailable rather than return an empty, apparently complete object.
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!(
            "component {} does not expose its configuration; retain its external binding",
            self.descriptor().id()
        )
    }
    /// Opt in to the graph's resolved incoming bindings
    /// ([`Self::bind_inputs`]). A component supplied already constructed has no
    /// factory context, so this is how it learns the actual producing output and
    /// stream of each input; it must not infer a producer from payloads or
    /// stream spelling. Factory-created components read
    /// [`super::ConstructionContext::input_bindings`] instead.
    fn binds_inputs(&self) -> bool {
        false
    }
    /// Called only for an opted-in component, before every start and before each
    /// processing operation, with the validated declared incoming edges ordered by
    /// input port then producer output. The call is repeated (with possibly
    /// different bindings) after a live rebind, and implementations compare
    /// with what they hold. It happens at an operation boundary, never during
    /// data handling. An error fails that operation; it is never ignored.
    async fn bind_inputs(&mut self, _bindings: &[super::InputBinding]) -> anyhow::Result<()> {
        Ok(())
    }
    /// Supplies a generation-bound sender. This is optional for components that
    /// do not originate control notifications.
    fn bind_control(&mut self, _control: super::ComponentControl) {}
    /// The shared handler is polled independently of mutable data operations.
    fn control_handler(&self) -> Option<std::sync::Arc<dyn super::ControlHandler>> {
        None
    }
    /// Nested/legacy hosts may finish their start hook before their underlying
    /// component is ready. Such hosts explicitly report `control.ready()`.
    fn requires_readiness_confirmation(&self) -> bool {
        false
    }
    async fn start(&mut self) -> anyhow::Result<()>;
    async fn stop(&mut self) -> anyhow::Result<()>;
    /// Erase owned state for explicit permanent removal, after stopping work.
    /// Ordinary stop, replacement and instance shutdown do not call this hook.
    async fn deprovision(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("this component does not support permanent state removal")
    }
    /// Called only at a processing boundary when the registered factory explicitly
    /// supports in-place configuration changes. The descriptor cannot change.
    async fn reconfigure(&mut self, _context: super::ConstructionContext) -> anyhow::Result<()> {
        anyhow::bail!("this component does not support in-place reconfiguration")
    }
}

/// A constructed service or nested graph with lifecycle but no data ports.
/// Its work runs in an independently scheduled, graph-owned Tokio task.
#[async_trait]
pub trait ComputationService: ComputationComponent {
    async fn run(&mut self) -> anyhow::Result<()>;

    /// Called after `run()` is dropped for quiescence. Services with separately
    /// owned workers must await their pause here and resume them on the next run.
    /// Services whose work lives entirely in `run()` need no additional action.
    async fn quiesce(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A producer beside legacy Source. Its descriptor has output ports only.
#[async_trait]
pub trait EnvelopeSource: ComputationComponent {
    /// Read capability used by this source. External readers are not evidence
    /// of a local owner; the host adapter must retain its actual bound resource.
    fn recovery_reader(&self) -> Option<super::SourceProgressReader> {
        self.recovery_progress().map(Into::into)
    }
    /// Delegate ingress to graph-validated publication into the actual outgoing
    /// QoS journal. When present, the graph drives this handle instead of `next`.
    /// Fast sources return None and retain their existing data path.
    fn admission(&self) -> Option<std::sync::Arc<super::SourceAdmission>> {
        None
    }

    /// The immediate consumer whose committed input permits this source to
    /// resume/prune history. The host rejects routing through another component
    /// (or fanout to another boundary) while using this consumer's checkpoint.
    fn recovery_progress(&self) -> Option<std::sync::Arc<super::QuerySourceProgress>> {
        None
    }

    /// Wait for the next event; None means exhausted, never temporarily idle.
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>>;
}

/// A stateful transformer with declared input and output ports.
#[async_trait]
pub trait Transformer: ComputationComponent {
    /// Called after startup and before processing/replay, with the graph's actual
    /// receipt-tracked destinations. Participating producers persist these using
    /// their existing transaction and reject changes while output is unconfirmed.
    /// The default does not claim durable destination tracking.
    async fn bind_output_destinations(
        &mut self,
        bindings: &super::OutputBindings,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            bindings.destinations().is_empty(),
            "this transformer does not support durable output destination tracking"
        );
        Ok(())
    }

    /// Called only after every outgoing branch accepted every envelope in this
    /// batch. A transformer relying on durable acceptance must require it on its
    /// output ports. This is not downstream handling or external-effect completion.
    ///
    /// The host also calls this for wakeups and continuations, before continuing
    /// or acknowledging the input. Failure/cancellation must retain recoverable
    /// pending output; the host never claims that an interrupted hook succeeded.
    async fn delivery_completed(&mut self, _outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        Ok(())
    }

    /// A bounded continuation of the current input. The host finishes these
    /// emissions before accepting another input or acknowledging this one.
    fn has_pending_emissions(&self) -> bool {
        false
    }
    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::bail!("this transformer has no continuation")
    }

    fn wakeup_source(&self) -> Option<std::sync::Arc<dyn WakeupSource>> {
        None
    }
    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::bail!("this transformer does not implement scheduled processing")
    }
    /// Produce zero, one, or many ordered emissions. Use ChangeEnvelope::derive to
    /// preserve input context and lineage, with this producer's output sequence.
    ///
    /// Success means local transformation completed, not that outputs have been
    /// enqueued/handled or the input acknowledged. The host calls
    /// `delivery_completed` after all branches accept. Cancellation/error can
    /// leave state changed; atomic staging/recovery remains component-specific.
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>>;
}

/// A consumer beside legacy Reaction. Its descriptor has input ports only.
#[async_trait]
pub trait EnvelopeSink: ComputationComponent {
    /// Read only at an operation boundary. The graph owns bounded collection,
    /// timers and delivery receipts; no success is reported merely for buffering.
    fn batching(&self) -> anyhow::Result<Option<SinkBatching>> {
        Ok(None)
    }
    async fn handle_batch(&mut self, _inputs: Vec<InputEnvelope>) -> anyhow::Result<()> {
        anyhow::bail!("this sink does not implement host-owned batching")
    }
    /// The handler that replaces this sink's state from a snapshot on `port`, if
    /// that input registered one. Capability is per input port and read at
    /// construction/operation boundaries: it must perform no I/O and must not
    /// appear or disappear after activation. `None` means snapshots on that
    /// port are unsupported and are rejected, never silently accepted.
    fn snapshot_consumer(&mut self, _port: &PortId) -> Option<&mut dyn SnapshotConsumer> {
        None
    }
    /// Immutable for this component instance. A legacy Reaction enqueue adapter
    /// must declare Accepted, never Handled. ExplicitAcknowledgement on a pipe
    /// cannot upgrade this declaration.
    fn completion(&self) -> SinkCompletion;

    /// Success meets exactly the instance's declared completion boundary.
    /// It does not automatically acknowledge a Delivery or establish
    /// durability/atomicity of external effects.
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()>;
}
