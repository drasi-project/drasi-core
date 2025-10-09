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

pub mod application;
pub mod grpc;
pub mod grpc_adaptive;
pub mod http;
pub mod http_adaptive;
pub mod log;
pub mod manager;
pub mod platform;
pub mod sse;

#[cfg(test)]
mod tests;

pub use application::{ApplicationReaction, ApplicationReactionHandle};
pub use grpc::GrpcReaction;
pub use grpc_adaptive::AdaptiveGrpcReaction;
pub use http::HttpReaction;
pub use http_adaptive::AdaptiveHttpReaction;
pub use log::LogReaction;
pub use manager::*;
pub use platform::PlatformReaction;
pub use sse::SseReaction;
