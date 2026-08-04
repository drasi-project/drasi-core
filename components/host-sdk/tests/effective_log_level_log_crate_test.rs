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

//! Pins `effective_host_log_level()` for `log`-crate-only hosts (env_logger
//! style: no tracing subscriber installed). The host's `log` max level is
//! reported as-is, except `Off`, which is indistinguishable from "no logger
//! ever installed" at the `log` crate's public API and therefore biases
//! toward forwarding — such hosts opt into silence via
//! `LoadedPlugin::set_log_level(Off)` instead.
//!
//! Own file: depends on process-global state (no tracing subscriber may be
//! installed, `log` max level is mutated).

use drasi_host_sdk::callbacks::effective_host_log_level;
use drasi_plugin_sdk::ffi::FfiLogLevelFilter;

#[test]
fn effective_level_falls_back_to_log_crate_level() {
    // env_logger-style host: `log::set_max_level` called, no tracing at all.
    log::set_max_level(log::LevelFilter::Warn);
    assert_eq!(
        effective_host_log_level(),
        FfiLogLevelFilter::Warn,
        "without a tracing subscriber the host must report the log crate's level"
    );

    // `Off` cannot be distinguished from "never configured", so it reports
    // Trace (forward everything) rather than silencing plugins by default.
    log::set_max_level(log::LevelFilter::Off);
    assert_eq!(
        effective_host_log_level(),
        FfiLogLevelFilter::Trace,
        "log-level Off is ambiguous with unconfigured and must bias toward forwarding"
    );
}
