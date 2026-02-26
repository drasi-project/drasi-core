use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::process::Command;

fn main() {
    // Capture the rustc version at build time
    let rustc_version = Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=DRASI_RUSTC_VERSION={}", rustc_version);

    // Capture the target triple for plugin metadata validation
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=TARGET_TRIPLE={}", target);

    // Compute build compatibility hash from rustc version, crate version,
    // target triple, and build profile. Used to reject plugins built with
    // a different toolchain or configuration.
    let crate_version = env!("CARGO_PKG_VERSION");
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string());

    let mut hasher = DefaultHasher::new();
    rustc_version.hash(&mut hasher);
    crate_version.hash(&mut hasher);
    target.hash(&mut hasher);
    profile.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());

    println!("cargo:rustc-env=DRASI_BUILD_HASH={hash}");

    // Rerun if the compiler or profile changes
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=PROFILE");
}
