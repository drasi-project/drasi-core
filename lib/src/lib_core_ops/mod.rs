// Copyright 2025 The Drasi Authors.
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

//! Core implementation modules for DrasiLib
//!
//! This module contains the implementation details for DrasiLib operations,
//! organized by component type. The main `DrasiLib` struct delegates to
//! these modules for specific functionality.
//!
//! # Module Organization
//!
//! - `source_ops`: Source management operations (add, remove, start, stop)
//! - `query_ops`: Query management operations (create, remove, start, stop)
//! - `reaction_ops`: Reaction management operations (add, remove, start, stop)

mod query_ops;
mod reaction_ops;
mod source_ops;

// These modules add impl blocks to DrasiLib, they don't export types
// The modules are compiled but we don't need to re-export anything
