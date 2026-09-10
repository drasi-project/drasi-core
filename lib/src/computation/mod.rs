// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Experimental computation contracts and runtime, enabled by the default-off `computation`
//! Cargo feature. These sit beside the existing [`crate::Source`] and
//! [`crate::Reaction`] contracts. Opt-in [`crate::DrasiLib`] methods host native
//! graphs without migrating its legacy ComponentGraph pipeline.
//!
//! Only versioned namespaces are public. [`v1::ComputationGraph`] runs native
//! components over capability-checked pipes, with explicit adapters for existing
//! plugins and graph-owned query recovery. Server configuration and plugin loading
//! remain the embedding host's responsibility; legacy plugin ABIs are unchanged.

pub(crate) mod instance;
mod instance_ops;
mod internal;

pub mod v1;
