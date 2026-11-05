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

//! Component logs, tracing integration and bounded lifecycle-event history.
//!
//! These utilities observe components; they do not own component membership,
//! processing or lifecycle.

#[cfg(test)]
#[cfg(feature = "otel-context")]
mod async_tests;
mod bootstrap;
mod component_log;
mod detail_metrics;
mod event_history;
mod log_runtime;
mod logging;
#[cfg(test)]
mod metric_tests;
mod native_spans;
mod operations;
mod plugin_telemetry;
mod queues;
mod telemetry;
mod tracing_layer;
mod worker;

pub use bootstrap::BootstrapTrace;
pub use component_log::*;
pub(crate) use detail_metrics::*;
pub use event_history::*;
pub use log_runtime::*;
pub use logging::*;
pub use native_spans::*;
pub(crate) use operations::*;
pub use queues::QueueObservations;
pub(crate) use queues::{discarded_pending, pending_depth, QueueBinding};
pub use telemetry::*;
pub use tracing_layer::*;
pub use worker::{ReactionInput, ReactionWork, WorkerObservation, WorkerTelemetry};
