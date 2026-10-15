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

//! Protocol buffer definitions for gRPC reactions.
//!
//! Compatibility imports for the shared reaction protocol.

/// Generated protobuf code for drasi.v1
pub use drasi_grpc_common::reaction as drasi_v1;

// Re-export commonly used types for convenience
pub use drasi_v1::{
    reaction_service_client::ReactionServiceClient, ProcessResultsRequest, ProcessResultsResponse,
    QueryResult as ProtoQueryResult, QueryResultItem as ProtoQueryResultItem,
};
