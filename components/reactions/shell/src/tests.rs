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
    use crate::config::{
        QueryConfig, ShellCommand, ShellExtension, ShellReactionConfig, TemplateSpec,
    };
    use crate::executor::ShellExecutor;
    use crate::shell::ShellReaction;
    use crate::state::{RecentInvocation, RecentInvocations};
    use crate::ShellReactionBuilder;
    use chrono::Utc;
    use drasi_lib::channels::ComponentStatus;
    use drasi_lib::Reaction;
    use std::collections::HashMap;

    fn sh_cmd() -> ShellCommand {
        ShellCommand {
            executable: "/bin/sh".to_string(),
            args: vec![],
        }
    }

    fn single_query_config(query_id: &str) -> ShellReactionConfig {
        let mut commands = HashMap::new();
        commands.insert(query_id.to_string(), sh_cmd());
        ShellReactionConfig {
            max_concurrent: 10,
            max_stdin_bytes: 4096,
            max_recent_invocations: 5,
            capture_limit: 1024,
            timeout_s: 30,
            kill_on_drop: true,
            env: HashMap::new(),
            routes: HashMap::new(),
            commands,
            default_template: Some(QueryConfig::default()),
        }
    }

    //.........................ShellReaction tests

    #[tokio::test]
    async fn reaction_new_starts_stopped() {
        let config = single_query_config("q1");
        let reaction = ShellReaction::new("shell-test", vec!["q1".to_string()], config).unwrap();
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn reaction_new_exposes_id_and_queries() {
        let config = single_query_config("q1");
        let reaction = ShellReaction::new("my-shell", vec!["q1".to_string()], config).unwrap();
        assert_eq!(reaction.id(), "my-shell");
        assert_eq!(reaction.query_ids(), vec!["q1".to_string()]);
    }

    #[tokio::test]
    async fn reaction_new_fails_on_invalid_config() {
        let mut config = single_query_config("q1");
        config.max_concurrent = 0; // invalid
        let result = ShellReaction::new("shell-test", vec!["q1".to_string()], config);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn reaction_type_name_is_shell() {
        let config = single_query_config("q1");
        let reaction = ShellReaction::new("t", vec!["q1".to_string()], config).unwrap();
        assert_eq!(reaction.type_name(), "shell");
    }

    //.......................ShellReactionBuilder tests

    #[tokio::test]
    async fn builder_creates_reaction_with_defaults() {
        let reaction = ShellReactionBuilder::new("builder-test")
            .with_query("q1")
            .with_command("q1", sh_cmd())
            .with_default_template(QueryConfig::default())
            .build()
            .unwrap();

        assert_eq!(reaction.id(), "builder-test");
        assert_eq!(reaction.query_ids(), vec!["q1".to_string()]);
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn builder_with_multiple_queries() {
        let reaction = ShellReactionBuilder::new("multi-q")
            .with_query("sensors")
            .with_query("alerts")
            .with_command("sensors", sh_cmd())
            .with_command("alerts", sh_cmd())
            .with_default_template(QueryConfig::default())
            .build()
            .unwrap();

        let mut ids = reaction.query_ids();
        ids.sort();
        assert_eq!(ids, vec!["alerts".to_string(), "sensors".to_string()]);
    }

    #[tokio::test]
    async fn builder_with_per_query_route_and_no_default() {
        let route = QueryConfig {
            added: Some(TemplateSpec {
                template: "[ADD] {{after.id}}".to_string(),
                extension: ShellExtension::default(),
            }),
            updated: None,
            deleted: None,
        };

        let reaction = ShellReactionBuilder::new("route-test")
            .with_query("q1")
            .with_command("q1", sh_cmd())
            .with_route("q1", route)
            .build()
            .unwrap();

        assert_eq!(reaction.id(), "route-test");
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn builder_with_global_env_vars() {
        let reaction = ShellReactionBuilder::new("env-test")
            .with_query("q1")
            .with_command("q1", sh_cmd())
            .with_default_template(QueryConfig::default())
            .with_env("GLOBAL_KEY", "global_value")
            .build()
            .unwrap();

        assert_eq!(reaction.id(), "env-test");
    }

    #[tokio::test]
    async fn builder_fails_when_no_command_for_query() {
        let result = ShellReactionBuilder::new("invalid")
            .with_query("q1")
            .with_default_template(QueryConfig::default())
            .build();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn builder_fails_when_command_for_unsubscribed_query() {
        let result = ShellReactionBuilder::new("invalid")
            .with_query("q1")
            .with_command("q1", sh_cmd())
            .with_command("q_extra", sh_cmd())
            .with_default_template(QueryConfig::default())
            .build();
        assert!(result.is_err());
    }

    //..................properties() tests

    #[tokio::test]
    async fn properties_contains_expected_config_keys() {
        let config = single_query_config("q1");
        let reaction = ShellReaction::new("props-test", vec!["q1".to_string()], config).unwrap();

        let props = reaction.properties();

        assert!(props.contains_key("max_concurrent"));
        assert!(props.contains_key("max_stdin_bytes"));
        assert!(props.contains_key("capture_limit"));
        assert!(props.contains_key("timeout_s"));
        assert!(props.contains_key("kill_on_drop"));
        assert!(props.contains_key("max_recent_invocations"));
    }

    #[tokio::test]
    async fn properties_contains_state_keys() {
        let config = single_query_config("q1");
        let reaction = ShellReaction::new("state-props", vec!["q1".to_string()], config).unwrap();

        let props = reaction.properties();
        assert!(props.contains_key("active_processes"));
        assert!(props.contains_key("timeout_firings"));
        assert!(props.contains_key("non_zero_exits"));
        assert!(props.contains_key("recent_invocations"));
    }

    #[tokio::test]
    async fn properties_config_values_match() {
        let mut config = single_query_config("q1");
        config.max_concurrent = 42;
        config.timeout_s = 99;
        let reaction = ShellReaction::new("values-test", vec!["q1".to_string()], config).unwrap();

        let props = reaction.properties();
        assert_eq!(props["max_concurrent"], serde_json::json!(42u32));
        assert_eq!(props["timeout_s"], serde_json::json!(99u64));
    }

    //...................more tests.

    #[test]
    fn config_serializes_and_deserializes() {
        let mut commands = HashMap::new();
        commands.insert("q1".to_string(), sh_cmd());

        let config = ShellReactionConfig {
            max_concurrent: 5,
            max_stdin_bytes: 2048,
            max_recent_invocations: 20,
            capture_limit: 512,
            timeout_s: 10,
            kill_on_drop: false,
            env: {
                let mut e = HashMap::new();
                e.insert("MY_VAR".to_string(), "val".to_string());
                e
            },
            routes: HashMap::new(),
            commands,
            default_template: Some(QueryConfig {
                added: Some(TemplateSpec {
                    template: "{{after.id}}".to_string(),
                    extension: ShellExtension::default(),
                }),
                updated: None,
                deleted: None,
            }),
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ShellReactionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, config);
    }

    #[test]
    fn config_deserializes_with_defaults_when_fields_omitted() {
        let json = serde_json::json!({
            "commands": {
                "q1": { "executable": "/bin/sh" }
            },
            "default_template": {}
        })
        .to_string();

        let config: ShellReactionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            config.max_concurrent,
            crate::config::default_max_concurrent()
        );
        assert_eq!(config.timeout_s, crate::config::default_timeout_s());
        assert_eq!(config.kill_on_drop, crate::config::default_kill_on_drop());
    }

    //..........................RecentInvocations tests

    #[test]
    fn recent_invocations_respects_capacity() {
        let mut ring = RecentInvocations::new(3);

        for i in 0..5u32 {
            ring.add_invocation(RecentInvocation {
                start_timestamp: Utc::now(),
                end_timestamp: None,
                exit_status: Some(i as i32),
                stdout: format!("out{i}"),
                stderr: String::new(),
                executable: "/bin/sh".to_string(),
            });
        }

        // Should only hold the 3 most recent (pushed front, so index 0 is newest)
        assert_eq!(ring.invocations.len(), 3);
        assert_eq!(ring.invocations[0].exit_status, Some(4));
        assert_eq!(ring.invocations[1].exit_status, Some(3));
        assert_eq!(ring.invocations[2].exit_status, Some(2));
    }

    #[test]
    fn recent_invocations_zero_capacity_stays_empty() {
        let mut ring = RecentInvocations::new(0);
        ring.add_invocation(RecentInvocation {
            start_timestamp: Utc::now(),
            end_timestamp: None,
            exit_status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            executable: "/bin/sh".to_string(),
        });
        assert!(ring.invocations.is_empty());
    }

    // read_limited tests

    #[tokio::test]
    async fn read_limited_returns_all_bytes_when_under_limit() {
        let data = b"hello world";
        let cursor = std::io::Cursor::new(data);
        let (out, truncated) = ShellExecutor::read_limited(cursor, 100).await.unwrap();
        assert_eq!(out, b"hello world");
        assert!(!truncated);
    }

    #[tokio::test]
    async fn read_limited_truncates_when_over_limit() {
        let data = b"hello world";
        let cursor = std::io::Cursor::new(data);
        let (out, truncated) = ShellExecutor::read_limited(cursor, 5).await.unwrap();
        assert_eq!(out, b"hello");
        assert!(truncated);
    }

    #[tokio::test]
    async fn read_limited_returns_empty_for_empty_input() {
        let cursor = std::io::Cursor::new(b"");
        let (out, truncated) = ShellExecutor::read_limited(cursor, 100).await.unwrap();
        assert!(out.is_empty());
        assert!(!truncated);
    }

    #[tokio::test]
    async fn read_limited_exact_boundary_not_truncated() {
        let data = b"abcde";
        let cursor = std::io::Cursor::new(data);
        let (out, truncated) = ShellExecutor::read_limited(cursor, 5).await.unwrap();
        assert_eq!(out, b"abcde");
        assert!(!truncated);
    }
}
