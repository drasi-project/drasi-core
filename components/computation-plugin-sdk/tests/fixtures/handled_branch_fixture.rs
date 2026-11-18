// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[path = "handled_branch_fixture_common.rs"]
mod common;
drasi_computation_plugin_sdk::export_computation_plugin!(common::definition());
