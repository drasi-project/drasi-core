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

//! `RUST_LOG=off` must be honored at the producer: a subscriber configured
//! for silence reports `Off` to plugins, which then forward nothing. Without
//! the installed-subscriber check this case is indistinguishable from "no
//! subscriber" and falls through to the `log` crate's max level — which the
//! host's own LogTracer bridge pins at `Trace`, making the host that wants
//! the least logging pay for the most forwarding.
//!
//! Own file: mutates process-global state (env var, global subscriber).

use drasi_host_sdk::callbacks::effective_host_log_level;
use drasi_plugin_sdk::ffi::FfiLogLevelFilter;

#[test]
fn effective_level_honors_rust_log_off() {
    std::env::set_var("RUST_LOG", "off");
    let _registry = drasi_lib::get_or_init_global_registry();

    assert_eq!(
        effective_host_log_level(),
        FfiLogLevelFilter::Off,
        "with RUST_LOG=off the host must report Off so plugins forward nothing"
    );
}
