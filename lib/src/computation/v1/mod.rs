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

//! Version 1 of the experimental, default-off computation contracts and DAG runtime.
//!
//! Enable with `drasi-lib = { version = "0.9", features = ["computation"] }`.
//! [`ComputationGraph`] runs owned native sources, transformers and sinks without
//! a Continuous Query. [`BoundedPipe`] supplies volatile FIFO/backpressure.
//! Legacy source/query/reaction execution remains unchanged.
//!
//! Every data-plane boundary carries [`ChangeEnvelope`]: a shared immutable
//! [`ChangeEvent`] describing a set diff and a branch-owned appendable list of
//! immutable context entries. [`ChangeEnvelope::append_annotation`] extends only
//! that envelope's history. Clones share the event and existing annotations;
//! [`ChangeEnvelope::derive`] creates a new event without changing its input.
//! [`Envelope`] remains a compatibility name for the same envelope type.
//!
//! Records contain immutable, schema-defined bytes. A [`RecordValidator`] must
//! check their encoding, domain constraints, and embedded identity where applicable.
//! [`RecordReference`] validates schema-specific keys even for image-less deletes.
//! The library additionally checks complete schema descriptors, operation ordering,
//! image roles, and cross-image identity. Providers are responsible for the truth
//! of their validators and capability declarations.
//!
//! Records/envelopes do not implement automatic serialization. [`EnvelopeCodec`]
//! explicitly encodes versioned computation storage and validates registered
//! schemas when decoding. Record payload encoding is schema-specific, not a
//! legacy Drasi wire standard. [`GraphChangeCodec`] and [`QueryChangeCodec`] preserve
//! typed graph/query boundaries, while explicit source/reaction adapters retain
//! legacy APIs. SDK/ABI integration is separate work.
//!
//! ```
//! use drasi_lib::computation::v1::{
//!     PipeCapabilities, PipeCapability, PipeRequirements, SchemaVersion,
//! };
//! use std::num::NonZeroUsize;
//!
//! assert!(SchemaVersion::try_new(0).is_err());
//! let pipe = PipeCapabilities::volatile_bounded(NonZeroUsize::new(16).unwrap());
//! let durable = PipeRequirements::new([PipeCapability::DurableAcceptance]);
//! assert!(pipe.validate(&durable).is_err());
//! ```
//!
//! # Scoped runtime lifecycle
//!
//! Build validates the *entire* DAG before pipe creation or lifecycle hooks.
//! [`ComputationGraph::start`] exclusively borrows the graph and returns a
//! caller-polled [`GraphRun`]; constructing it alone runs nothing. Consumers are
//! considered before producers, but activation is independent unless a relationship
//! explicitly requires a running dependency. Every eligible component is attempted.
//! Futures for different nodes run concurrently, with no detached graph tasks.
//! Each component's mutable operations remain serialized. Components/providers
//! must be cooperative, and must not block the runtime thread.
//!
//! `Ready -> Starting -> Running -> Stopping -> Completed` is the clean path.
//! Only `Completed` can restart with retained component ownership, retained
//! sequence high-watermarks and fresh pipes. A native component's start hook may
//! reset its finite input cursor, but must not regress its output stream sequence.
//! Startup reports retain per-component successes and failures. Failed activation
//! does not erase desired nodes, stop independent components, or close bound pipes.
//! [`ComputationGraph::run`] separates deployment from activation and accepts live
//! revision-checked lifecycle/policy commands through [`GraphControl`].
//! Component generations and operation epochs reject stale health observations.
//! [`ComponentSpecification`] and [`ComponentFactory`] separate desired definitions
//! from constructed instances. Configuration, reference, role, implementation and
//! resource-cardinality checks run before construction. [`ResourceSpecification`]
//! describes scoped ownership; [`ResourceHandle`] holds an actual instance outside
//! desired snapshots. [`ComputationGraph::dispose`] awaits owned-resource cleanup.
//! Cancellation is not rollback. Cleanup errors preserve their causes and leave
//! `CleanupRequired`.
//!
//! For prompt cancellation, call [`GraphControl::cancel`] and keep awaiting the
//! run. It drops pending data operations before awaiting stop hooks, without a
//! component lock. Cleanup has a configurable shared deadline and attempts every
//! hook. Dropping a run drops all scoped operations and closes pipes immediately,
//! but cannot await async hooks: call [`ComputationGraph::shutdown`] afterwards.
//! Dropping the graph without explicit cleanup cannot stop resources spawned by
//! component implementations. No async cleanup is claimed on `Drop`.
//!
//! # Routing and volatile boundaries
//!
//! Each output port explicitly binds one unique producer stream. Emissions must
//! use that stream and an increasing sequence, irrespective of timestamp. Full
//! schema descriptors and all outputs in a transform result are checked before
//! forwarding any of that result. Opaque logical envelope IDs remain caller-owned;
//! duplicates within a result are rejected, but there is no unbounded global ID
//! deduplication index. [`emission_id`] is an optional stream/sequence ID convention.
//!
//! Every declared port must connect; sources have outputs only, sinks inputs only,
//! and transformers both. Complete disconnected pipelines are allowed. Graph IDs
//! are local, not registered process-wide. Exact duplicate edges, cycles,
//! unbounded pipes and unsupported delivery capabilities are errors. Broadcast
//! lag is explicit. Retained providers name their actual registered store resource
//! and acknowledge local handling separately from enqueue acceptance.
//! A producer output may not route through multiple input ports of the same
//! component; independent queues for that one stream could violate its FIFO.
//!
//! Fanout sends sequentially in edge declaration order, shares immutable payloads,
//! and preserves branch-local context. A slow branch applies backpressure to its
//! producer (not isolation from other branches). Fanin preserves each stream's
//! FIFO, not a global order. [`InputMergePolicy::EventTimeAcrossStreams`] orders
//! currently available stream heads without violating that FIFO or waiting on an
//! unavailable source. Only actual finite exhaustion closes outgoing pipes,
//! so filters and fanin drain naturally even if providers retained sender clones.
//! Failed or stopped producers leave their bindings idle, not falsely exhausted.
//! Queue capacity counts queued envelopes, not bytes, component-local state,
//! in-flight processing, or a transform's caller-allocated result vector.
//!
//! Acceptance is not handling, acknowledgement or durability. Operational errors
//! remain visible on their component; independent components remain activated.
//! Contract violations and forwarding errors terminate the run with their causes.
//! A forwarding error reports prior branch acceptances;
//! cancellation can also leave partial fanout or external effects. Neither is
//! retried or rolled back. Explicit pipe close drains accepted events; cancellation
//! may discard volatile queued/in-flight events. [`RetainedPipe`] preserves
//! unacknowledged stored work; durable providers distinguish uncertain commit
//! outcomes rather than claiming definite rejection. Generic cross-component
//! transactions and exactly-once external effects are not inferred from these
//! profiles. [`ContinuousQueryTransformer`] owns query evaluation/publication and
//! scheduled work; [`LegacyReactionSink`] remains acceptance-only.

mod bounded_pipe;
mod broadcast_pipe;
mod codec;
mod component;
mod consumer_recovery;
mod data;
mod envelope;
mod error;
mod graph;
mod graph_codec;
mod legacy_reaction;
mod legacy_source;
mod lifecycle;
mod pipe;
mod pipe_metrics;
mod ports;
mod query;
mod query_bootstrap;
mod query_codec;
mod retained_pipe;
mod retained_store;
mod wal_source;

pub use bounded_pipe::*;
pub use broadcast_pipe::*;
pub use codec::*;
pub use component::*;
pub use consumer_recovery::*;
pub use data::*;
pub use envelope::*;
pub use error::{ContractError, Result};
pub use graph::*;
pub use graph_codec::*;
pub use legacy_reaction::*;
pub use legacy_source::*;
pub use lifecycle::*;
pub use pipe::*;
pub use pipe_metrics::PipeMetricsSnapshot;
pub use ports::*;
pub use query::*;
pub use query_bootstrap::*;
pub use query_codec::*;
pub use retained_pipe::*;
pub use retained_store::*;
pub use wal_source::*;
