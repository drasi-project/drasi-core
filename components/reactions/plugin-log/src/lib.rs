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
mod log;

#[cfg(test)]
mod tests;

pub use config::LogReactionConfig;
pub use log::LogReaction;

// Extension trait for plugin registration
use drasi_lib::plugin_core::ReactionRegistry;
use std::sync::Arc;

pub trait ReactionRegistryLogExt {
    fn register_log(&mut self);
}

impl ReactionRegistryLogExt for ReactionRegistry {
    fn register_log(&mut self) {
        self.register("log".to_string(), |config, event_tx| {
            Ok(Arc::new(LogReaction::new(config, event_tx)))
        });
    }
}