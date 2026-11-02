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

//! `RUST_LOG=off` is a log-only policy even when routing/operation spans raise
//! the subscriber's maximum. The LogTracer bridge's facade level is not policy.
//!
//! Own file: mutates process-global state (env var, global subscriber).

use drasi_host_sdk::callbacks::effective_host_log_level;
use drasi_plugin_sdk::ffi::FfiLogLevelFilter;

#[test]
fn effective_level_honors_rust_log_off() {
    std::env::set_var("RUST_LOG", "off");
    let runtime = drasi_lib::init_default_subscriber().unwrap();

    assert_eq!(
        effective_host_log_level(),
        FfiLogLevelFilter::Off,
        "with RUST_LOG=off the host must report Off so plugins forward nothing"
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(runtime.shutdown(std::time::Duration::from_secs(2)))
        .unwrap();
}
