// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

fn main() {
    if let Err(error) = drasi_instance_host::main_entry() {
        eprintln!("drasi-instance-host: {error:#}");
        std::process::exit(1);
    }
}
