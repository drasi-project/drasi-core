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

//! TODO: This test module needs to be rewritten for the plugin architecture.
//! The original tests used DrasiServerCore::builder() API which is no longer available.

#[cfg(test)]
mod tests {
    use crate::LogReaction;
    use drasi_lib::channels::ComponentStatus;
    use drasi_lib::config::{ReactionConfig, ReactionSpecificConfig};
    use drasi_lib::reactions::Reaction;
    use serde_json::json;
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    /// Helper to convert a serde_json::Value object to HashMap<String, serde_json::Value>
    fn to_hashmap(value: serde_json::Value) -> HashMap<String, serde_json::Value> {
        match value {
            serde_json::Value::Object(map) => map.into_iter().collect(),
            _ => HashMap::new(),
        }
    }

    #[tokio::test]
    async fn test_log_reaction_creation() {
        let (event_tx, _event_rx) = mpsc::channel(100);

        let config = ReactionConfig {
            id: "test-log".to_string(),
            queries: vec!["query1".to_string()],
            auto_start: false,
            config: ReactionSpecificConfig::Log(to_hashmap(json!({
                "log_level": "info"
            }))),
            priority_queue_capacity: None,
        };

        let reaction = LogReaction::new(config, event_tx);
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }
}
