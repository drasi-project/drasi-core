// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[cfg(any(feature = "source", feature = "reaction"))]
use std::path::PathBuf;

#[cfg(any(feature = "source", feature = "reaction"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is missing")?);
    if cfg!(feature = "source") {
        let directory = output.join("source");
        std::fs::create_dir_all(&directory)?;
        tonic_prost_build::configure()
            .out_dir(directory)
            .compile_protos(
                &[
                    "proto/source/drasi/v1/source.proto",
                    "proto/source/drasi/v1/common.proto",
                ],
                &["proto/source"],
            )?;
    }
    if cfg!(feature = "reaction") {
        let directory = output.join("reaction");
        std::fs::create_dir_all(&directory)?;
        tonic_prost_build::configure()
            .out_dir(directory)
            .compile_protos(
                &["proto/reaction/drasi/v1/reaction.proto"],
                &["proto/reaction"],
            )?;
    }
    Ok(())
}

#[cfg(not(any(feature = "source", feature = "reaction")))]
fn main() {}
