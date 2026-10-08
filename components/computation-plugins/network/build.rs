// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Package the imported declaration so crates.io builds need no workspace.
    // Workspace builds reject drift; generated Rust still uses the shared DTOs.
    let shared = "../../sources/grpc/proto/drasi/v1/common.proto";
    if std::path::Path::new(shared).try_exists()? {
        println!("cargo:rerun-if-changed={shared}");
        if std::fs::read(shared)? != std::fs::read("proto/drasi/v1/common.proto")? {
            return Err(
                "native admission's packaged common.proto differs from the shared source protocol"
                    .into(),
            );
        }
    }
    tonic_prost_build::configure()
        .extern_path(".drasi.v1", "crate::proto::source")
        .compile_protos(&["proto/admission.proto"], &["proto"])?;
    Ok(())
}
