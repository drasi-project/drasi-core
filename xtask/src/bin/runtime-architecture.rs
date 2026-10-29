// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![allow(clippy::print_stdout, clippy::print_stderr)]

#[path = "../runtime_architecture/mod.rs"]
mod report;

fn main() {
    if let Err(error) = report::run() {
        eprintln!("runtime-architecture: {error}");
        std::process::exit(1);
    }
}
