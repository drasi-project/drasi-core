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

mod config;
mod mock;

#[cfg(test)]
mod tests;

pub use config::MockSourceConfig;
pub use mock::MockSource;

// Extension trait for plugin registration
use drasi_lib::plugin_core::SourceRegistry;
use std::sync::Arc;

pub trait SourceRegistryMockExt {
    fn register_mock(&mut self);
}

impl SourceRegistryMockExt for SourceRegistry {
    fn register_mock(&mut self) {
        self.register("mock".to_string(), |config, event_tx| {
            Ok(Arc::new(MockSource::new(config, event_tx)?))
        });
    }
}