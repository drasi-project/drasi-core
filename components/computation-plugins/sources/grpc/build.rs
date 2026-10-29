// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .extern_path(".drasi.v1", "crate::proto::source")
        .compile_protos(
            &["proto/admission.proto"],
            &["proto", drasi_grpc_common::SOURCE_PROTO_ROOT],
        )?;
    Ok(())
}
