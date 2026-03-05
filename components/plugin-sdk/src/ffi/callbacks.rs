// Copyright 2025 The Drasi Authors.
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

//! Log and lifecycle callback types for host↔plugin communication.

use std::ffi::c_void;

use super::types::FfiStr;

// ============================================================================
// Log capture — host provides callback, plugins emit logs through it
// ============================================================================

/// Log level for FFI log entries.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiLogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

/// A log entry emitted by a plugin, delivered to the host via callback.
/// All FfiStr fields are borrowed and only valid for the duration of the callback.
#[repr(C)]
pub struct FfiLogEntry {
    pub level: FfiLogLevel,
    pub plugin_id: FfiStr,
    pub target: FfiStr,
    pub message: FfiStr,
    pub timestamp_us: i64,
    /// DrasiLib instance ID (set from FfiRuntimeContext during initialize).
    /// Empty if not yet initialized.
    pub instance_id: FfiStr,
    /// Component instance ID (e.g., "my-sensor", not the plugin kind).
    /// Empty if not yet initialized.
    pub component_id: FfiStr,
}

/// Callback function the host provides for capturing plugin logs.
///
/// The `ctx` pointer is an opaque host-owned context passed to the plugin
/// via `set_log_callback`. The plugin stores it alongside the callback and
/// passes it back on every invocation, allowing the host to route logs to
/// the correct DrasiLib instance.
pub type LogCallbackFn = extern "C" fn(ctx: *mut c_void, entry: *const FfiLogEntry);

// ============================================================================
// Lifecycle event capture — host observes component state transitions
// ============================================================================

/// Lifecycle event types for component state transitions.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiLifecycleEventType {
    Starting = 0,
    Started = 1,
    Stopping = 2,
    Stopped = 3,
    Error = 4,
}

/// A lifecycle event emitted when a component changes state.
/// All FfiStr fields are borrowed and only valid for the duration of the callback.
#[repr(C)]
pub struct FfiLifecycleEvent {
    pub component_id: FfiStr,
    /// Component type string (e.g., "source", "reaction", "bootstrap").
    pub component_type: FfiStr,
    pub event_type: FfiLifecycleEventType,
    /// Error message (empty for non-error events).
    pub message: FfiStr,
    pub timestamp_us: i64,
}

/// Callback function the host provides for capturing lifecycle events.
///
/// The `ctx` pointer is an opaque host-owned context (same as for `LogCallbackFn`).
pub type LifecycleCallbackFn = extern "C" fn(ctx: *mut c_void, event: *const FfiLifecycleEvent);
