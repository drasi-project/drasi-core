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

    println!("cargo:rustc-env=DRASI_RUSTC_VERSION={rustc_version}");

    // Capture the target triple for plugin metadata validation
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=TARGET_TRIPLE={target}");

    // Capture the git commit SHA (short hash) for plugin provenance.
    // In dev builds, use a stable placeholder to avoid cache invalidation on every commit.
    let git_commit = if std::env::var("PROFILE").as_deref() == Ok("release")
        || std::env::var("DRASI_EMBED_GIT_SHA").is_ok()
    {
        Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    } else {
        "dev".to_string()
    };
    println!("cargo:rustc-env=GIT_COMMIT_SHA={git_commit}");

    // Capture the build timestamp in RFC 3339 UTC format.
    // In dev builds, use a stable placeholder to avoid invalidating the cache on every build.
    // The timestamp changes every second, which would force recompilation of plugin-sdk
    // and all 27+ downstream crates on every `cargo build`.
    let build_timestamp = if std::env::var("PROFILE").as_deref() == Ok("release")
        || std::env::var("DRASI_EMBED_BUILD_TIMESTAMP").is_ok()
    {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    } else {
        "dev-build".to_string()
    };
    println!("cargo:rustc-env=BUILD_TIMESTAMP={build_timestamp}");

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

    // Rerun if the compiler or profile changes.
    // In release builds, also rerun when git HEAD changes for accurate provenance.
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=PROFILE");
    println!("cargo:rerun-if-env-changed=DRASI_EMBED_BUILD_TIMESTAMP");
    println!("cargo:rerun-if-env-changed=DRASI_EMBED_GIT_SHA");
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        println!("cargo:rerun-if-changed=.git/HEAD");
    }
}
