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

//! Configuration types for file reaction.

use drasi_lib::reactions::common::{self, TemplateRouting};
use std::collections::HashMap;

pub use common::{QueryConfig, TemplateSpec};

/// File write behavior for emitted query result diffs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum WriteMode {
    /// Appends each rendered record to the destination file.
    #[default]
    Append,
    /// Replaces destination file content with the latest rendered record.
    Overwrite,
    /// Writes each rendered record to a unique file.
    PerChange,
}

fn default_output_path() -> String {
    ".".to_string()
}

/// Configuration for the file reaction.
#[derive(Debug, Clone, PartialEq)]
pub struct FileReactionConfig {
    /// Base directory for all output files.
    pub output_path: String,

    /// How output should be persisted.
    pub write_mode: WriteMode,

    /// Handlebars filename template.
    ///
    /// If omitted, the reaction uses a default template based on `write_mode`.
    pub filename_template: Option<String>,

    /// Query-specific templates.
    pub routes: HashMap<String, QueryConfig>,

    /// Default templates used when no query-specific route is configured.
    pub default_template: Option<QueryConfig>,
}

impl Default for FileReactionConfig {
    fn default() -> Self {
        Self {
            output_path: default_output_path(),
            write_mode: WriteMode::Append,
            filename_template: None,
            routes: HashMap::new(),
            default_template: None,
        }
    }
}

impl TemplateRouting for FileReactionConfig {
    fn routes(&self) -> &HashMap<String, QueryConfig> {
        &self.routes
    }

    fn default_template(&self) -> Option<&QueryConfig> {
        self.default_template.as_ref()
    }
}
