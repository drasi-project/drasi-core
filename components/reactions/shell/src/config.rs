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

#![cfg(target_os = "linux")]

use core::error;
use drasi_lib::reactions::common::TemplateRouting;
pub use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use crate::descriptor::ShellReactionConfigDto;
use crate::ShellReaction;

const REGEX_ENV_VAR: &str = r"^[A-Za-z_][A-Za-z0-9_]*$";

pub fn default_max_concurrent() -> u32 {
    100
}

pub fn default_max_stdin_bytes() -> usize {
    1024 * 1024 // 1 MB
}

pub fn default_capture_limit() -> usize {
    1024 * 4 // 4 KB
}

pub fn default_timeout_s() -> u64 {
    60 // 60 seconds
}

pub fn default_kill_on_drop() -> bool {
    true
}

pub fn default_max_recent_invocations() -> u32 {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ShellCommand {
    pub executable: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ShellExtension {
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShellReactionConfig {
    /// Maximum number of concurrent shell commands. Default: 100.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
    /// Maximum bytes to send to stdin otherwise the command won't be executed. Default: 1 MB.
    #[serde(default = "default_max_stdin_bytes")]
    pub max_stdin_bytes: usize,
    /// Maximum number of recent invocations to keep track of. Default: 10.
    #[serde(default = "default_max_recent_invocations")]
    pub max_recent_invocations: u32,

    /// Maximum bytes to capture from stdout/stderr. Default: 4 KB.
    #[serde(default = "default_capture_limit")]
    pub capture_limit: usize,
    /// Timeout for each shell command in seconds. Default: 60 seconds.
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u64,
    /// Whether to kill the command if the reaction is dropped. Default: true.
    #[serde(default = "default_kill_on_drop")]
    pub kill_on_drop: bool,

    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub env: HashMap<String, String>,
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub routes: HashMap<String, QueryConfig<ShellExtension>>,
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub commands: HashMap<String, ShellCommand>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig<ShellExtension>>,
}

impl Default for ShellReactionConfig {
    fn default() -> Self {
        Self {
            max_concurrent: default_max_concurrent(),
            max_stdin_bytes: default_max_stdin_bytes(),
            max_recent_invocations: default_max_recent_invocations(),
            capture_limit: default_capture_limit(),
            timeout_s: default_timeout_s(),
            kill_on_drop: default_kill_on_drop(),
            env: HashMap::new(),
            routes: HashMap::new(),
            commands: HashMap::new(),
            default_template: Some(QueryConfig::default()),
        }
    }
}

impl TemplateRouting<ShellExtension> for ShellReactionConfig {
    fn routes(&self) -> &HashMap<String, QueryConfig<ShellExtension>> {
        &self.routes
    }

    fn default_template(&self) -> Option<&QueryConfig<ShellExtension>> {
        self.default_template.as_ref()
    }
}

impl ShellReactionConfig {
    pub fn validate(
        &self,
        subscribed_queries: &Vec<String>,
        reaction_id: &str,
    ) -> anyhow::Result<()> {
        // validate the single controlling values.
        if self.max_concurrent == 0 {
            error!("[{reaction_id}] Invalid configuration: max_concurrent must be greater than 0");
            Err(anyhow::anyhow!("max_concurrent must be greater than 0"))?;
        }
        if self.max_stdin_bytes == 0 {
            error!("[{reaction_id}] Invalid configuration: max_stdin_bytes must be greater than 0");
            Err(anyhow::anyhow!("max_stdin_bytes must be greater than 0"))?;
        }
        if self.capture_limit == 0 {
            error!("[{reaction_id}] Invalid configuration: capture_limit must be greater than 0");
            Err(anyhow::anyhow!("capture_limit must be greater than 0"))?;
        }
        if self.timeout_s == 0 {
            error!("[{reaction_id}] Invalid configuration: timeout_s must be greater than 0");
            Err(anyhow::anyhow!("timeout_s must be greater than 0"))?;
        }
        if self.max_recent_invocations == 0 {
            warn!("[{reaction_id}] Warning: max_recent_invocations is set to 0, no invocation history will be retained");
        }

        // check that number of commands matches the number of subscribed queries (size check).
        if (self.commands.len() != subscribed_queries.len()) {
            error!("[{reaction_id}] Invalid configuration: number of commands must match the number of subscribed queries. commands: {}, subscribed_queries: {}", self.commands.len(), subscribed_queries.len());
            Err(anyhow::anyhow!(
                "number of commands must match the number of subscribed queries"
            ))?;
        }

        // check that all commands reference valid query ids
        for (query_id, command) in &self.commands {
            if !subscribed_queries.contains(query_id) {
                error!("[{reaction_id}] Invalid configuration: command defined for query_id '{query_id}' which is not in the list of subscribed queries");
                Err(anyhow::anyhow!("command defined for query_id '{query_id}' which is not in the list of subscribed queries"))?;
            } else {
                // validate the command executable.
                self.validate_command(command, reaction_id)?;
            }
        }

        // check that the number of routes if less that or equal to the number of subscribed queries
        if self.routes.len() > self.commands.len() {
            error!("[{reaction_id}] Invalid configuration: number of routes cannot exceed the number of subscribed queries. routes: {}, subscribed_queries: {}", self.routes.len(), subscribed_queries.len());
            Err(anyhow::anyhow!(
                "number of routes cannot exceed the number of subscribed queries"
            ))?;
        }

        // compile env regex once to use in multiple places
        let re = regex::Regex::new(REGEX_ENV_VAR)?;

        let mut counter = 0;
        for router in &self.routes {
            if !self.commands.contains_key(router.0) {
                // check the query_id
                error!("[{reaction_id}] Invalid configuration: route defined for query_id '{}' that the reaction is not subscribed to", router.0);
                Err(anyhow::anyhow!(
                    "route defined for query_id '{}' that the reaction is not subscribed to",
                    router.0
                ))?;
            } else {
                counter += 1;

                // validate the env var names in the route extensions
                if let Some(template) = &router.1.added {
                    for env in template.extension.env.keys() {
                        self.validate_env_var(env.clone(), reaction_id, &re)?;
                    }
                }
                if let Some(template) = &router.1.updated {
                    for env in template.extension.env.keys() {
                        self.validate_env_var(env.clone(), reaction_id, &re)?;
                    }
                }
                if let Some(template) = &router.1.deleted {
                    for env in template.extension.env.keys() {
                        self.validate_env_var(env.clone(), reaction_id, &re)?;
                    }
                }
            }
        }

        if counter != self.commands.len() && self.default_template.is_none() {
            error!("[{reaction_id}] Invalid configuration: not all commands have a corresponding route and no default template is defined. commands: {}, routes: {}, default_template: {}", self.commands.len(), self.routes.len(), self.default_template.is_some());
            Err(anyhow::anyhow!(
                "not all commands have a corresponding route and no default template is defined"
            ))?;
        }

        // validate env var names
        self.validate_env_vars(reaction_id, &re)?;

        Ok(())
    }

    fn validate_command(&self, command: &ShellCommand, reaction_id: &str) -> anyhow::Result<()> {
        let exec = &command.executable;

        // validate that the executable is not empty
        if exec.is_empty() {
            error!("[{reaction_id}] Invalid configuration: command executable cannot be empty");
            return Err(anyhow::anyhow!("command executable cannot be empty"));
        }

        // validate that the executable does not contain spaces
        if exec.contains(' ') {
            error!(
                "[{reaction_id}] Invalid configuration: command executable cannot contain spaces"
            );
            return Err(anyhow::anyhow!("command executable cannot contain spaces"));
        }

        let path = std::path::Path::new(exec);

        if path.is_absolute() {
            if !path.exists() {
                error!("[{reaction_id}] Invalid configuration: command executable '{exec}' does not exist");
                return Err(anyhow::anyhow!(
                    "command executable '{exec}' does not exist"
                ));
            }

            if !path.is_file() {
                error!("[{reaction_id}] Invalid configuration: command executable '{exec}' is not a file");
                return Err(anyhow::anyhow!("command executable '{exec}' is not a file"));
            }

            // permissions check
            let metadata = std::fs::metadata(exec)?;
            let mode = metadata.permissions().mode();
            let file_uid = metadata.uid();
            let file_gid = metadata.gid();

            let current_user_uid = nix::unistd::getuid().as_raw();
            let current_user_gid = nix::unistd::getgid().as_raw();

            let can_execute = if current_user_uid == 0 {
                // root can execute any file that has at least one of the execute bits set
                mode & 0o111 != 0
            } else if current_user_uid == file_uid {
                // owner permissions
                mode & 0o100 != 0
            } else if current_user_gid == file_gid {
                // group permissions
                mode & 0o010 != 0
            } else {
                // other permissions
                mode & 0o001 != 0
            };

            if !can_execute {
                error!("[{reaction_id}] Invalid configuration: command executable '{exec}' is not executable");
                return Err(anyhow::anyhow!(
                    "command executable '{exec}' is not executable"
                ));
            }
        } else {
            error!("[{reaction_id}] Invalid configuration: command executable '{exec}' must be an absolute path to the executable");
            return Err(anyhow::anyhow!(
                "command executable '{exec}' must be an absolute path to the executable"
            ));
        }

        // validate the args aren't empty
        for arg in &command.args {
            if arg.is_empty() {
                error!("[{reaction_id}] Invalid configuration: command arguments cannot be empty");
                return Err(anyhow::anyhow!("command arguments cannot be empty"));
            }
        }

        Ok(())
    }

    fn validate_env_vars(&self, reaction_id: &str, re: &regex::Regex) -> anyhow::Result<()> {
        // validate env var names in the main config
        for env in self.env.keys() {
            self.validate_env_var(env.clone(), reaction_id, re)?;
        }

        // skip validating env var names in the route extensions since they are already validated in the validate function when iterating through the routes.
        // validate env vars in default template if exist
        if let Some(default_template) = &self.default_template {
            if let Some(template) = &default_template.added {
                for env in template.extension.env.keys() {
                    self.validate_env_var(env.clone(), reaction_id, re)?;
                }
            }
            if let Some(template) = &default_template.updated {
                for env in template.extension.env.keys() {
                    self.validate_env_var(env.clone(), reaction_id, re)?;
                }
            }
            if let Some(template) = &default_template.deleted {
                for env in template.extension.env.keys() {
                    self.validate_env_var(env.clone(), reaction_id, re)?;
                }
            }
        }
        Ok(())
    }

    fn validate_env_var(
        &self,
        env: String,
        reaction_id: &str,
        re: &regex::Regex,
    ) -> anyhow::Result<()> {
        // validate against REGEX_ENV_VAR
        if !re.is_match(&env) {
            error!("[{reaction_id}] Invalid environment variable name: {env}");
            return Err(anyhow::anyhow!("Invalid environment variable name: {env}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh_cmd() -> ShellCommand {
        ShellCommand {
            executable: "/bin/sh".to_string(),
            args: vec![],
        }
    }

    fn minimal_config(query_ids: &[&str]) -> (ShellReactionConfig, Vec<String>) {
        let subscribed: Vec<String> = query_ids.iter().map(|s| s.to_string()).collect();
        let mut commands = HashMap::new();
        for id in query_ids {
            commands.insert(id.to_string(), sh_cmd());
        }
        let config = ShellReactionConfig {
            max_concurrent: 100,
            max_stdin_bytes: 1024 * 1024,
            max_recent_invocations: 100,
            capture_limit: 4 * 1024,
            timeout_s: 60,
            kill_on_drop: true,
            env: HashMap::new(),
            routes: HashMap::new(),
            commands,
            default_template: Some(QueryConfig::default()),
        };
        (config, subscribed)
    }

    #[test]
    fn validate_succeeds_with_valid_config() {
        let (config, subscribed) = minimal_config(&["q1"]);
        assert!(config.validate(&subscribed, "test").is_ok());
    }

    #[test]
    fn validate_fails_with_missing_default_config() {
        let (mut config, subscribed) = minimal_config(&["q1", "q2"]);
        config.default_template = None; // no default template
        assert!(config.validate(&subscribed, "test").is_err()); // and no routes → invalid
    }

    //........................... check the numeric limits.

    #[test]
    fn validate_fails_when_max_concurrent_is_zero() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config.max_concurrent = 0;
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err.to_string().contains("max_concurrent"));
    }

    #[test]
    fn validate_fails_when_max_stdin_bytes_is_zero() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config.max_stdin_bytes = 0;
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err.to_string().contains("max_stdin_bytes"));
    }

    #[test]
    fn validate_fails_when_capture_limit_is_zero() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config.capture_limit = 0;
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err.to_string().contains("capture_limit"));
    }

    #[test]
    fn validate_fails_when_timeout_s_is_zero() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config.timeout_s = 0;
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err.to_string().contains("timeout_s"));
    }

    #[test]
    fn validate_succeeds_when_max_recent_invocations_is_zero() {
        // zero is allowed – it just means no history is retained (warns, no error)
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config.max_recent_invocations = 0;
        assert!(config.validate(&subscribed, "test").is_ok());
    }

    //............................ check routing, commands and subscription validation logic

    #[test]
    fn validate_fails_when_command_count_mismatches_subscribed_queries() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config.commands.insert("q2".to_string(), sh_cmd()); // extra command
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err.to_string().contains("number of commands"));
    }

    #[test]
    fn validate_fails_when_command_references_unknown_query() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config.commands.remove("q1");
        config.commands.insert("q_unknown".to_string(), sh_cmd());
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err.to_string().contains("q_unknown"));
    }

    #[test]
    fn validate_fails_when_routes_exceed_commands() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config
            .routes
            .insert("q1".to_string(), QueryConfig::default());
        config
            .routes
            .insert("q2".to_string(), QueryConfig::default());
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err.to_string().contains("routes"));
    }

    #[test]
    fn validate_fails_when_there_is_route_for_unsubscribed_query() {
        let (mut config, subscribed) = minimal_config(&["q1", "q2"]);
        config
            .routes
            .insert("q3".to_string(), QueryConfig::default()); // no command for q3
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err.to_string().contains("q3"));
    }

    #[test]
    fn validate_fails_when_partial_routes_and_no_default_template() {
        let (mut config, subscribed) = minimal_config(&["q1", "q2"]);
        config.default_template = None;
        config
            .routes
            .insert("q1".to_string(), QueryConfig::default()); // q2 has no route
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err.to_string().contains("not all commands"));
    }

    #[test]
    fn validate_succeeds_when_partial_routes_with_default_template() {
        let (mut config, subscribed) = minimal_config(&["q1", "q2"]);
        // default_template already set by minimal_config; add a route for only one query
        config
            .routes
            .insert("q1".to_string(), QueryConfig::default());
        assert!(config.validate(&subscribed, "test").is_ok());
    }

    #[test]
    fn validate_succeeds_when_all_commands_have_routes_and_no_default_template() {
        let (mut config, subscribed) = minimal_config(&["q1", "q2"]);
        config.default_template = None;
        config
            .routes
            .insert("q1".to_string(), QueryConfig::default());
        config
            .routes
            .insert("q2".to_string(), QueryConfig::default());
        assert!(config.validate(&subscribed, "test").is_ok());
    }

    //................................executable validation checks

    #[test]
    fn validate_fails_when_executable_is_empty() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config.commands.get_mut("q1").unwrap().executable = String::new();
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err.to_string().contains("executable cannot be empty"));
    }

    #[test]
    fn validate_fails_when_executable_contains_spaces() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config.commands.get_mut("q1").unwrap().executable = "my script".to_string();
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err.to_string().contains("cannot contain spaces"));
    }

    #[test]
    fn validate_fails_when_absolute_path_does_not_exist() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config.commands.get_mut("q1").unwrap().executable =
            "/nonexistent/drasi/test/executable".to_string();
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn validate_fails_when_absolute_path_is_not_executable() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join("drasi_test_non_exec_config");
        std::fs::write(&path, b"not a script").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644); // rw-r--r--  (no execute bits)
        std::fs::set_permissions(&path, perms).unwrap();

        let (mut config, subscribed) = minimal_config(&["q1"]);
        config.commands.get_mut("q1").unwrap().executable = path.to_str().unwrap().to_string();

        let result = config.validate(&subscribed, "test");
        std::fs::remove_file(&path).ok();

        let err = result.unwrap_err();
        assert!(err.to_string().contains("is not executable"));
    }

    #[test]
    fn validate_fails_when_executable_is_not_absolute_path() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config.commands.get_mut("q1").unwrap().executable = "sh".to_string();
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err.to_string().contains("absolute path"));
    }

    #[test]
    fn validate_fails_when_command_has_empty_arg() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config
            .commands
            .get_mut("q1")
            .unwrap()
            .args
            .push(String::new());
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err.to_string().contains("arguments cannot be empty"));
    }

    //................................environment variable name validation checks

    #[test]
    fn validate_succeeds_with_valid_env_var_names() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config.env.insert("MY_VAR".to_string(), "a".to_string());
        config.env.insert("_PRIVATE".to_string(), "b".to_string());
        config.env.insert("VAR123".to_string(), "c".to_string());
        assert!(config.validate(&subscribed, "test").is_ok());
    }

    #[test]
    fn validate_fails_with_env_var_starting_with_digit() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config
            .env
            .insert("1INVALID".to_string(), "value".to_string());
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err
            .to_string()
            .contains("Invalid environment variable name"));
    }

    #[test]
    fn validate_fails_with_env_var_containing_dash() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config.env.insert("MY-VAR".to_string(), "value".to_string());
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err
            .to_string()
            .contains("Invalid environment variable name"));
    }

    #[test]
    fn validate_fails_with_invalid_env_var_in_route_added_extension() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        let mut env = HashMap::new();
        env.insert("BAD NAME".to_string(), "v".to_string());
        config.routes.insert(
            "q1".to_string(),
            QueryConfig {
                added: Some(TemplateSpec {
                    template: String::new(),
                    extension: ShellExtension { env },
                }),
                updated: None,
                deleted: None,
            },
        );
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err
            .to_string()
            .contains("Invalid environment variable name"));
    }

    #[test]
    fn validate_fails_with_invalid_env_var_in_default_template_extension() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        let mut env = HashMap::new();
        env.insert("INVALID-NAME".to_string(), "v".to_string());
        config.default_template = Some(QueryConfig {
            added: Some(TemplateSpec {
                template: String::new(),
                extension: ShellExtension { env },
            }),
            updated: None,
            deleted: None,
        });
        let err = config.validate(&subscribed, "test").unwrap_err();
        assert!(err
            .to_string()
            .contains("Invalid environment variable name"));
    }

    #[test]
    fn validate_succeeds_with_duplicate_env_var_names() {
        let (mut config, subscribed) = minimal_config(&["q1"]);
        config.env.insert("DUPLICATE".to_string(), "v1".to_string());
        config.env.insert("DUPLICATE".to_string(), "v2".to_string());
        let result = config.validate(&subscribed, "test");
        assert!(result.is_ok(), "Validation should succeed even with duplicate env var names since HashMap keys are unique and the duplicate will just overwrite the previous value. Error: {:?}", result.err());
        assert!(
            config.env.contains_key("DUPLICATE"),
            "The env var 'DUPLICATE' should exist in the config env"
        );
        if let Some(value) = config.env.get("DUPLICATE") {
            assert_eq!(value, "v2", "The value for the 'DUPLICATE' env var should be 'v2' since it overwrote the previous value");
        }
    }
}
