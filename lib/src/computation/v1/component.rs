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

use super::{ComponentDescriptor, Envelope, PortId};

/// Fixed meaning of a sink's successful handle call. This is an instance-level
/// declaration, never a per-call downgrade or a consequence of pipe capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkCompletion {
    /// Accepted by the sink or a legacy queue. No claim of durable acceptance,
    /// completed processing, or completed external effects.
    Accepted,
    /// Processing completed at the declared sink boundary. Not automatic
    /// end-to-end acknowledgement, atomic external effects, or exactly-once.
    Handled,
}

/// Input routed to a declared input port. The host validates port/schema membership.
#[derive(Debug, Clone)]
pub struct InputEnvelope {
    pub port: PortId,
    pub envelope: Envelope,
}

/// New emission on a declared output port. The producing component owns its
/// output stream and authoritative monotonically increasing sequence.
#[derive(Debug, Clone)]
pub struct OutputEnvelope {
    pub port: PortId,
    pub envelope: Envelope,
}

/// Host-owned component lifecycle, independent of legacy Source/Reaction contexts.
///
/// The future DAG host owns `Box<dyn ...>` instances, validates roles/ports before
/// starting, serializes all mutable calls, and owns cancellation and error policy.
/// Call processing methods only after successful start; drain/cancel processing
/// before stop. Dropping a cancelled future does not imply rollback of component
/// state or external effects. Restart, recovery, and transactional lifecycle are
/// not supplied by these contracts or by Drasi Server.
///
/// The descriptor must remain unchanged throughout the owned component's life.
/// No default success methods conceal missing implementation.
#[async_trait]
pub trait ComputationComponent: Send + Sync {
    fn descriptor(&self) -> &ComponentDescriptor;
    async fn start(&mut self) -> anyhow::Result<()>;
    async fn stop(&mut self) -> anyhow::Result<()>;
}

/// A producer beside legacy Source. Its descriptor has output ports only.
#[async_trait]
pub trait EnvelopeSource: ComputationComponent {
    /// Wait for the next event; None means exhausted, never temporarily idle.
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>>;
}

/// A stateful transformer with declared input and output ports.
#[async_trait]
pub trait Transformer: ComputationComponent {
    /// Produce zero, one, or many ordered emissions. Use Envelope::derive to
    /// preserve input context and lineage, with this producer's output sequence.
    ///
    /// Success means local transformation completed, not that outputs have been
    /// enqueued/handled or the input acknowledged. Cancellation/error can leave
    /// component-local state changed; atomic staging/recovery is deferred.
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>>;
}

/// A consumer beside legacy Reaction. Its descriptor has input ports only.
#[async_trait]
pub trait EnvelopeSink: ComputationComponent {
    /// Immutable for this component instance. A legacy Reaction enqueue adapter
    /// must declare Accepted, never Handled. ExplicitAcknowledgement on a pipe
    /// cannot upgrade this declaration.
    fn completion(&self) -> SinkCompletion;

    /// Success meets exactly the instance's declared completion boundary.
    /// It does not automatically acknowledge a Delivery or establish
    /// durability/atomicity of external effects.
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()>;
}
