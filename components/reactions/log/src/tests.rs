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

#[cfg(test)]
mod tests {
    use crate::{LogReaction, LogReactionConfig};
    use drasi_lib::channels::ComponentStatus;
    use drasi_lib::plugin_core::Reaction;

    #[tokio::test]
    async fn test_log_reaction_creation() {
        let config = LogReactionConfig::default();

        let reaction = LogReaction::new("test-log", vec!["query1".to_string()], config);
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_with_templates() {
        let config = LogReactionConfig {
            added_template: Some("[NEW] Item {{after.id}}".to_string()),
            updated_template: Some("[CHG] {{before.value}} -> {{after.value}}".to_string()),
            deleted_template: Some("[DEL] Item {{before.id}}".to_string()),
        };

        let reaction = LogReaction::new("test-log-templates", vec!["query1".to_string()], config);
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_builder() {
        let reaction = LogReaction::builder("test-log-builder")
            .from_query("query1")
            .with_added_template("[ADD] {{after.name}}")
            .with_updated_template("[UPD] {{after.name}}")
            .with_deleted_template("[DEL] {{before.name}}")
            .build();

        assert_eq!(reaction.id(), "test-log-builder");
        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_builder_no_templates() {
        let reaction = LogReaction::builder("test-log-no-templates")
            .from_query("query1")
            .from_query("query2")
            .build();

        assert_eq!(reaction.id(), "test-log-no-templates");
        assert_eq!(
            reaction.query_ids(),
            vec!["query1".to_string(), "query2".to_string()]
        );
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }
}
