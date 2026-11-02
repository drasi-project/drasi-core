// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/orchestrator_service.proto");
    tonic_prost_build::configure()
        .build_server(true)
        .generate_default_stubs(true)
        .compile_protos(&["proto/orchestrator_service.proto"], &["proto"])?;
    Ok(())
}
