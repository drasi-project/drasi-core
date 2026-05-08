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
use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

pub fn default_max_concurrent() -> usize {
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

pub fn default_max_recent_invocations() -> usize {
    100
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
    pub envs: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShellReactionConfig {
    /// Maximum number of concurrent shell commands. Default: 100.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// Maximum bytes to send to stdin otherwise the command won't be executed. Default: 1 MB.
    #[serde(default = "default_max_stdin_bytes")]
    pub max_stdin_bytes: usize,
    /// Maximum number of recent invocations to keep track of. Default: 100.
    #[serde(default = "default_max_recent_invocations")]
    pub max_recent_invocations: usize,

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

        // check that all commands referenced in routes exist in the commands map
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
                "number `of routes cannot exceed the number of subscribed queries"
            ))?;
        }

        // compile env regex once to use in multiple places
        let re = regex::Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$")?;

        let mut counter = 0;
        for router in &self.routes {
            if !self.commands.contains_key(router.0) {
                error!("[{reaction_id}] Invalid configuration: route defined for query_id '{}' which does not have a corresponding command", router.0);
                Err(anyhow::anyhow!(
                    "route defined for query_id '{}' which does not have a corresponding command",
                    router.0
                ))?;
            } else {
                counter += 1;

                // validate the env var names in the route extensions
                if let Some(template) = &router.1.added {
                    for env in template.extension.envs.keys() {
                        self.validate_env_var(env.clone(), reaction_id, &re)?;
                    }
                }
                if let Some(template) = &router.1.updated {
                    for env in template.extension.envs.keys() {
                        self.validate_env_var(env.clone(), reaction_id, &re)?;
                    }
                }
                if let Some(template) = &router.1.deleted {
                    for env in template.extension.envs.keys() {
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
            // plain name like "python", "bash" to be searched in `PATH`
            let found = std::env::var("PATH")
                .unwrap_or_default()
                .split(':')
                .map(|dir| std::path::Path::new(dir).join(exec))
                .any(|p| p.exists());

            if !found {
                error!("[{reaction_id}] Invalid configuration: command executable '{exec}' not found in PATH");
                return Err(anyhow::anyhow!(
                    "command executable '{exec}' not found in PATH"
                ));
            }
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
                for env in template.extension.envs.keys() {
                    self.validate_env_var(env.clone(), reaction_id, re)?;
                }
            }
            if let Some(template) = &default_template.updated {
                for env in template.extension.envs.keys() {
                    self.validate_env_var(env.clone(), reaction_id, re)?;
                }
            }
            if let Some(template) = &default_template.deleted {
                for env in template.extension.envs.keys() {
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
        // validate against the regex [A-Za-z_][A-Za-z0-9_]*
        if !re.is_match(&env) {
            error!("[{reaction_id}] Invalid environment variable name: {env}");
            return Err(anyhow::anyhow!("Invalid environment variable name: {env}"));
        }
        Ok(())
    }
}
