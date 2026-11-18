// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Compiles the vendored OpenTelemetry protocol definitions (opentelemetry-proto
//! v1.5.0) with a vendored `protoc`, so building needs no system protobuf.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    println!("cargo:rerun-if-changed=proto");
    // Upstream proto comments contain JSON examples that rustdoc would try to
    // compile as doctests.
    let mut prost_config = prost_build::Config::new();
    prost_config.disable_comments(["."]);
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_with_config(
            prost_config,
            &[
                "proto/opentelemetry/proto/collector/trace/v1/trace_service.proto",
                "proto/opentelemetry/proto/collector/metrics/v1/metrics_service.proto",
                "proto/opentelemetry/proto/collector/logs/v1/logs_service.proto",
            ],
            &["proto"],
        )?;
    Ok(())
}
