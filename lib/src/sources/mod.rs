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

pub mod base;
pub mod future_queue_source;
pub mod manager;
mod traits;

#[cfg(test)]
pub(crate) mod tests;

use async_trait::async_trait;
use drasi_core::models::SourceChange;

/// Trait for publishing source changes to the Drasi server
#[async_trait]
pub trait Publisher: Send + Sync {
    /// Publish a source change event
    async fn publish(
        &self,
        change: SourceChange,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

// Re-export the Source trait
pub use traits::Source;

pub use base::{SourceBase, SourceBaseParams};
pub use future_queue_source::{FutureQueueSource, FUTURE_QUEUE_SOURCE_ID};
pub use manager::SourceManager;
pub use manager::{convert_json_to_element_properties, convert_json_to_element_value};
