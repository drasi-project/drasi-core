// Copyright 2026 The Drasi Authors.
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

#![allow(unexpected_cfgs)]

//! Durable GitHub WorkGraph worker-slot dispatcher Reaction.

pub mod config;
pub mod descriptor;
mod dispatcher;
mod github;
mod model;
mod reaction;

pub use config::GitHubWorkGraphDispatcherConfig;
pub use reaction::GitHubWorkGraphDispatcher;

pub struct GitHubWorkGraphDispatcherBuilder {
    id: String,
    queries: Vec<String>,
    config: GitHubWorkGraphDispatcherConfig,
    auto_start: bool,
}

impl GitHubWorkGraphDispatcherBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: GitHubWorkGraphDispatcherConfig::default(),
            auto_start: true,
        }
    }

    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.queries.push(query.into());
        self
    }

    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    pub fn with_config(mut self, config: GitHubWorkGraphDispatcherConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.config.token = token.into();
        self
    }

    pub fn with_api_url(mut self, api_url: impl Into<String>) -> Self {
        self.config.api_url = api_url.into();
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn build(self) -> anyhow::Result<GitHubWorkGraphDispatcher> {
        self.config.validate(&self.queries)?;
        Ok(GitHubWorkGraphDispatcher::from_builder(
            self.id,
            self.queries,
            self.config,
            self.auto_start,
        ))
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "github-workgraph-dispatcher-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::GitHubWorkGraphDispatcherDescriptor],
    bootstrap_descriptors = [],
);

#[cfg(test)]
mod tests;
