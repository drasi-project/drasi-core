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

//! Build script for drasi-plugin-runtime.
//!
//! Computes a build compatibility hash from:
//! - Rust compiler version
//! - Crate version
//! - Target triple
//! - Build profile (debug/release)
//!
//! This hash is embedded as `DRASI_BUILD_HASH` and used by the server to reject
//! plugins built with a different toolchain or configuration.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::process::Command;

fn main() {
    // Get rustc version
    let rustc_version = Command::new("rustc")
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let crate_version = env!("CARGO_PKG_VERSION");
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string());

    let mut hasher = DefaultHasher::new();
    rustc_version.hash(&mut hasher);
    crate_version.hash(&mut hasher);
    target.hash(&mut hasher);
    profile.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());

    println!("cargo:rustc-env=DRASI_BUILD_HASH={hash}");

    // Also expose rustc version for the existing version check in dynamic_loading.rs
    println!("cargo:rustc-env=DRASI_RUSTC_VERSION={rustc_version}");

    // Rerun if the compiler or profile changes
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=PROFILE");
}
