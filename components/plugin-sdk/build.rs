use std::process::Command;

fn main() {
    // Capture the rustc version at build time
    let rustc_version = Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_else(|| "unknown".to_string());

    println!(
        "cargo:rustc-env=DRASI_RUSTC_VERSION={}",
        rustc_version.trim()
    );

    // Capture the target triple for plugin metadata validation
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=TARGET_TRIPLE={}", target);
}
