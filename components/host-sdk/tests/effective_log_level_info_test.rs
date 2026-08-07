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

//! Pins the assumption producer-side log filtering rests on: with the real
//! drasi-lib logging stack installed (registry, EnvFilter, component layer,
//! fmt), `effective_host_log_level()` reflects `RUST_LOG`. If this ever
//! regressed to `Trace`, plugins would silently forward everything again.
//!
//! Lives in its own integration-test file because it mutates process-global
//! state (env var, global tracing subscriber); the `RUST_LOG=off` variant is
//! a separate file for the same reason.

use drasi_host_sdk::callbacks::effective_host_log_level;
use drasi_plugin_sdk::ffi::FfiLogLevelFilter;

#[test]
fn effective_level_tracks_env_filter() {
    // Fresh process, no subscriber, no `log` logger: forward everything so
    // bare test hosts still see all plugin records.
    assert_eq!(
        effective_host_log_level(),
        FfiLogLevelFilter::Trace,
        "without any logging configured the host must report Trace"
    );

    std::env::set_var("RUST_LOG", "info");
    let _registry = drasi_lib::get_or_init_global_registry();

    assert_eq!(
        effective_host_log_level(),
        FfiLogLevelFilter::Info,
        "with RUST_LOG=info the host must report Info, not Trace — \
         otherwise producer-side filtering never engages in production"
    );
}
