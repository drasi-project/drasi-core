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

/// Log level filter passed from the host to the plugin.
///
/// The host reports its effective log level through
/// `FfiPluginRegistration::set_log_level`. Records more verbose than this
/// level are dropped inside the plugin before they are formatted or cross
/// the FFI boundary, so filtered-out logging costs almost nothing.
///
/// Discriminants order by verbosity: `Off` forwards nothing, `Trace`
/// forwards everything.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiLogLevelFilter {
    Off = 0,
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl FfiLogLevelFilter {
    /// Convert to the `log` crate's `LevelFilter`.
    pub fn to_level_filter(self) -> log::LevelFilter {
        match self {
            FfiLogLevelFilter::Off => log::LevelFilter::Off,
            FfiLogLevelFilter::Error => log::LevelFilter::Error,
            FfiLogLevelFilter::Warn => log::LevelFilter::Warn,
            FfiLogLevelFilter::Info => log::LevelFilter::Info,
            FfiLogLevelFilter::Debug => log::LevelFilter::Debug,
            FfiLogLevelFilter::Trace => log::LevelFilter::Trace,
        }
    }

    /// Convert from the `log` crate's `LevelFilter`.
    pub fn from_level_filter(filter: log::LevelFilter) -> Self {
        match filter {
            log::LevelFilter::Off => FfiLogLevelFilter::Off,
            log::LevelFilter::Error => FfiLogLevelFilter::Error,
            log::LevelFilter::Warn => FfiLogLevelFilter::Warn,
            log::LevelFilter::Info => FfiLogLevelFilter::Info,
            log::LevelFilter::Debug => FfiLogLevelFilter::Debug,
            log::LevelFilter::Trace => FfiLogLevelFilter::Trace,
        }
    }

    /// Convert from a raw `u8` discriminant, clamping unknown values to `Trace`.
    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => FfiLogLevelFilter::Off,
            1 => FfiLogLevelFilter::Error,
            2 => FfiLogLevelFilter::Warn,
            3 => FfiLogLevelFilter::Info,
            4 => FfiLogLevelFilter::Debug,
            _ => FfiLogLevelFilter::Trace,
        }
    }

    /// Whether a record at `level` passes this filter.
    pub fn allows(self, level: FfiLogLevel) -> bool {
        // Exhaustive: a new FfiLogLevel variant is a compile error here
        // rather than a silent arithmetic drift between the two enums.
        let level_as_filter = match level {
            FfiLogLevel::Error => FfiLogLevelFilter::Error,
            FfiLogLevel::Warn => FfiLogLevelFilter::Warn,
            FfiLogLevel::Info => FfiLogLevelFilter::Info,
            FfiLogLevel::Debug => FfiLogLevelFilter::Debug,
            FfiLogLevel::Trace => FfiLogLevelFilter::Trace,
        };
        (level_as_filter as u8) <= self as u8
    }
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

// ============================================================================
// Config value resolver — host resolves ConfigValue references for plugins
// ============================================================================

/// Callback function the host provides for resolving `ConfigValue` references
/// (secrets, environment variables) in plugin configs.
///
/// The plugin serializes the `ConfigValue` to JSON (e.g.,
/// `{"kind":"Secret","name":"DB_PASSWORD"}`) and calls this function.
/// The host resolves the value using the appropriate store and returns the
/// resolved string via [`super::secret_store::FfiGetSecretResult`].
///
/// The `ctx` pointer is an opaque host-owned context containing the secret
/// store provider and any other resolution context.
pub type ConfigResolverFn = extern "C" fn(
    ctx: *const c_void,
    config_value_json: FfiStr,
) -> super::secret_store::FfiGetSecretResult;
