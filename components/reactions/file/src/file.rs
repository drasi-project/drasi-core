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

use crate::config::{FileReactionConfig, WriteMode};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use handlebars::Handlebars;
use log::{debug, error};
use lru::LruCache;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;

use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::reactions::common::{OperationType, TemplateRouting};
use drasi_lib::Reaction;

const FILE_LOCK_CACHE_SIZE: usize = 1024;

pub struct FileReaction {
    base: ReactionBase,
    config: FileReactionConfig,
    handlebars: Arc<Handlebars<'static>>,
    file_locks: Arc<Mutex<LruCache<String, Arc<Mutex<()>>>>>,
}

impl FileReaction {
    fn new_file_locks() -> Arc<Mutex<LruCache<String, Arc<Mutex<()>>>>> {
        Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(FILE_LOCK_CACHE_SIZE).expect("FILE_LOCK_CACHE_SIZE must be > 0"),
        )))
    }

    /// Create a builder for FileReaction.
    pub fn builder(id: impl Into<String>) -> FileReactionBuilder {
        FileReactionBuilder::new(id)
    }

    /// Create a new file reaction.
    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: FileReactionConfig,
    ) -> anyhow::Result<Self> {
        let id = id.into();
        Self::validate_config(&queries, &config)?;

        let params = ReactionBaseParams::new(id, queries);
        Ok(Self {
            base: ReactionBase::new(params),
            config,
            handlebars: Arc::new(Self::build_handlebars()),
            file_locks: Self::new_file_locks(),
        })
    }

    /// Create a new file reaction with custom priority queue capacity.
    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: FileReactionConfig,
        priority_queue_capacity: usize,
    ) -> anyhow::Result<Self> {
        let id = id.into();
        Self::validate_config(&queries, &config)?;

        let params = ReactionBaseParams::new(id, queries)
            .with_priority_queue_capacity(priority_queue_capacity);
        Ok(Self {
            base: ReactionBase::new(params),
            config,
            handlebars: Arc::new(Self::build_handlebars()),
            file_locks: Self::new_file_locks(),
        })
    }

    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: FileReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> anyhow::Result<Self> {
        Self::validate_config(&queries, &config)?;

        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }

        Ok(Self {
            base: ReactionBase::new(params),
            config,
            handlebars: Arc::new(Self::build_handlebars()),
            file_locks: Self::new_file_locks(),
        })
    }

    fn build_handlebars() -> Handlebars<'static> {
        let mut handlebars = Handlebars::new();
        handlebars.register_helper(
            "json",
            Box::new(
                |h: &handlebars::Helper,
                 _: &Handlebars,
                 _: &handlebars::Context,
                 _: &mut handlebars::RenderContext,
                 out: &mut dyn handlebars::Output|
                 -> handlebars::HelperResult {
                    if let Some(value) = h.param(0) {
                        let json_str =
                            serde_json::to_string(value.value()).unwrap_or_else(|_| "null".into());
                        out.write(&json_str)?;
                    }
                    Ok(())
                },
            ),
        );
        handlebars
    }

    fn default_filename_template(write_mode: &WriteMode) -> &'static str {
        match write_mode {
            WriteMode::Append => "{{query_name}}.log",
            WriteMode::Overwrite => "{{query_name}}.json",
            WriteMode::PerChange => "{{query_name}}_{{operation}}_{{uuid}}.json",
        }
    }

    fn validate_template(template: &str) -> anyhow::Result<()> {
        if template.is_empty() {
            return Ok(());
        }

        handlebars::Template::compile(template).map_err(|e| anyhow!("Invalid template: {e}"))?;
        Ok(())
    }

    fn validate_query_config(config: &crate::config::QueryConfig) -> anyhow::Result<()> {
        if let Some(added) = &config.added {
            Self::validate_template(&added.template)?;
        }
        if let Some(updated) = &config.updated {
            Self::validate_template(&updated.template)?;
        }
        if let Some(deleted) = &config.deleted {
            Self::validate_template(&deleted.template)?;
        }
        Ok(())
    }

    fn validate_config(queries: &[String], config: &FileReactionConfig) -> anyhow::Result<()> {
        if config.output_path.trim().is_empty() {
            return Err(anyhow!("output_path must not be empty"));
        }

        if let Some(filename_template) = &config.filename_template {
            Self::validate_template(filename_template)
                .map_err(|e| anyhow!("Invalid filename template: {e}"))?;
        } else {
            Self::validate_template(Self::default_filename_template(&config.write_mode))
                .map_err(|e| anyhow!("Invalid default filename template: {e}"))?;
        }

        for (query_id, route_config) in &config.routes {
            Self::validate_query_config(route_config)
                .map_err(|e| anyhow!("Invalid template in route '{query_id}': {e}"))?;
        }

        if let Some(default_template) = &config.default_template {
            Self::validate_query_config(default_template)
                .map_err(|e| anyhow!("Invalid default template: {e}"))?;
        }

        if !config.routes.is_empty() && !queries.is_empty() {
            for route_query in config.routes.keys() {
                let dotted_route = format!(".{route_query}");
                let matches = queries
                    .iter()
                    .any(|q| q == route_query || q.ends_with(&dotted_route));
                if !matches {
                    return Err(anyhow!(
                        "Route '{route_query}' does not match any subscribed query. Subscribed queries: {queries:?}"
                    ));
                }
            }
        }

        Ok(())
    }

    async fn get_file_lock(
        file_locks: &Arc<Mutex<LruCache<String, Arc<Mutex<()>>>>>,
        path: &Path,
    ) -> Arc<Mutex<()>> {
        let key = path.to_string_lossy().to_string();
        let mut locks = file_locks.lock().await;
        locks
            .get_or_insert(key, || Arc::new(Mutex::new(())))
            .clone()
    }

    async fn write_to_file(
        file_locks: &Arc<Mutex<LruCache<String, Arc<Mutex<()>>>>>,
        mode: &WriteMode,
        output_file: &Path,
        content: &str,
    ) -> Result<()> {
        if let Some(parent) = output_file.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let content = format!("{content}\n");

        match mode {
            WriteMode::Append => {
                let lock = Self::get_file_lock(file_locks, output_file).await;
                let _guard = lock.lock().await;
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(output_file)
                    .await?;
                file.write_all(content.as_bytes()).await?;
            }
            WriteMode::Overwrite => {
                let lock = Self::get_file_lock(file_locks, output_file).await;
                let _guard = lock.lock().await;
                // Write to a temporary file then atomically rename to avoid
                // exposing partially-written files to readers.
                let tmp_path = Self::tmp_path_for(output_file)?;
                tokio::fs::write(&tmp_path, content.as_bytes()).await?;
                tokio::fs::rename(&tmp_path, output_file).await?;
            }
            WriteMode::PerChange => {
                // No lock needed — each file is unique.
                // Still write atomically to avoid truncated files on crash.
                let tmp_path = Self::tmp_path_for(output_file)?;
                tokio::fs::write(&tmp_path, content.as_bytes()).await?;
                tokio::fs::rename(&tmp_path, output_file).await?;
            }
        }

        Ok(())
    }

    fn tmp_path_for(output_file: &Path) -> Result<PathBuf> {
        let file_name = output_file
            .file_name()
            .ok_or_else(|| anyhow!("output_file has no file name"))?;
        let tmp_file_name = format!("{}.tmp", file_name.to_string_lossy());
        let mut tmp_path = output_file.to_path_buf();
        tmp_path.set_file_name(tmp_file_name);
        Ok(tmp_path)
    }

    fn render_output(
        config: &FileReactionConfig,
        handlebars: &Handlebars<'static>,
        query_id: &str,
        event_timestamp: chrono::DateTime<chrono::Utc>,
        diff: &ResultDiff,
    ) -> Result<(PathBuf, String)> {
        let (operation_name, operation_type) = match diff {
            ResultDiff::Add { .. } => ("ADD", Some(OperationType::Add)),
            ResultDiff::Update { .. } => ("UPDATE", Some(OperationType::Update)),
            ResultDiff::Delete { .. } => ("DELETE", Some(OperationType::Delete)),
            ResultDiff::Aggregation { .. } => ("AGGREGATION", Some(OperationType::Update)),
            ResultDiff::Noop => ("NOOP", None),
        };

        let mut context = Map::new();
        context.insert("query_name".into(), Value::String(query_id.to_string()));
        context.insert(
            "operation".into(),
            Value::String(operation_name.to_string()),
        );
        context.insert(
            "timestamp".into(),
            Value::String(event_timestamp.to_rfc3339()),
        );
        context.insert("uuid".into(), Value::String(Uuid::new_v4().to_string()));

        match diff {
            ResultDiff::Add { data } => {
                context.insert("after".into(), data.clone());
                context.insert("data".into(), data.clone());
            }
            ResultDiff::Update {
                before,
                after,
                data,
                ..
            } => {
                context.insert("before".into(), before.clone());
                context.insert("after".into(), after.clone());
                context.insert("data".into(), data.clone());
            }
            ResultDiff::Delete { data } => {
                context.insert("before".into(), data.clone());
                context.insert("data".into(), data.clone());
            }
            ResultDiff::Aggregation { before, after } => {
                context.insert("before".into(), before.clone().unwrap_or(Value::Null));
                context.insert("after".into(), after.clone());
                context.insert("data".into(), after.clone());
            }
            ResultDiff::Noop => {}
        }

        let filename_template = config
            .filename_template
            .as_deref()
            .unwrap_or_else(|| Self::default_filename_template(&config.write_mode));

        let rendered_filename = handlebars.render_template(filename_template, &context)?;
        let sanitized_filename = sanitize_filename(&rendered_filename);
        if sanitized_filename.trim().is_empty() {
            return Err(anyhow!(
                "Rendered filename '{rendered_filename}' is empty after sanitization"
            ));
        }

        let content = if let Some(operation_type) = operation_type {
            if let Some(template_spec) = config.get_template_spec(query_id, operation_type) {
                handlebars.render_template(&template_spec.template, &context)?
            } else {
                serde_json::to_string(diff)?
            }
        } else {
            serde_json::to_string(diff)?
        };

        let output_file = Path::new(&config.output_path).join(sanitized_filename);
        Ok((output_file, content))
    }

    async fn process_result(
        config: &FileReactionConfig,
        handlebars: &Handlebars<'static>,
        file_locks: &Arc<Mutex<LruCache<String, Arc<Mutex<()>>>>>,
        query_result: &QueryResult,
        reaction_name: &str,
    ) {
        for diff in &query_result.results {
            if matches!(diff, ResultDiff::Noop) {
                continue;
            }
            match Self::render_output(
                config,
                handlebars,
                &query_result.query_id,
                query_result.timestamp,
                diff,
            ) {
                Ok((output_file, content)) => {
                    if let Err(e) =
                        Self::write_to_file(file_locks, &config.write_mode, &output_file, &content)
                            .await
                    {
                        error!(
                            "[{}] Failed to write output to '{}': {}",
                            reaction_name,
                            output_file.to_string_lossy(),
                            e
                        );
                    }
                }
                Err(e) => {
                    error!("[{reaction_name}] Failed to render output: {e}");
                }
            }
        }
    }
}

/// Replaces unsafe filename characters with `_` and escapes Windows reserved device names.
pub(crate) fn sanitize_filename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            _ => c,
        })
        .collect();

    // Check for Windows reserved device names (CON, PRN, AUX, NUL, COM1-9, LPT1-9).
    // These are reserved with or without an extension (e.g. "CON.json" is also invalid).
    let stem = sanitized.split('.').next().unwrap_or(&sanitized);
    let is_reserved = matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );

    if is_reserved {
        format!("_{sanitized}")
    } else {
        sanitized
    }
}

/// Builder for `FileReaction`.
pub struct FileReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: FileReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl FileReactionBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: FileReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    pub fn with_output_path(mut self, output_path: impl Into<String>) -> Self {
        self.config.output_path = output_path.into();
        self
    }

    pub fn with_write_mode(mut self, mode: WriteMode) -> Self {
        self.config.write_mode = mode;
        self
    }

    pub fn with_filename_template(mut self, filename_template: impl Into<String>) -> Self {
        self.config.filename_template = Some(filename_template.into());
        self
    }

    pub fn with_default_template(mut self, template: crate::config::QueryConfig) -> Self {
        self.config.default_template = Some(template);
        self
    }

    pub fn with_route(
        mut self,
        query_id: impl Into<String>,
        config: crate::config::QueryConfig,
    ) -> Self {
        self.config.routes.insert(query_id.into(), config);
        self
    }

    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn with_config(mut self, config: FileReactionConfig) -> Self {
        self.config = config;
        self
    }

    pub fn build(self) -> anyhow::Result<FileReaction> {
        FileReaction::from_builder(
            self.id,
            self.queries,
            self.config,
            self.priority_queue_capacity,
            self.auto_start,
        )
    }
}

#[async_trait]
impl Reaction for FileReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "file"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut properties = HashMap::new();
        properties.insert(
            "output_path".to_string(),
            Value::String(self.config.output_path.clone()),
        );
        properties.insert(
            "write_mode".to_string(),
            Value::String(
                match self.config.write_mode {
                    WriteMode::Append => "append",
                    WriteMode::Overwrite => "overwrite",
                    WriteMode::PerChange => "per_change",
                }
                .to_string(),
            ),
        );
        if let Some(filename_template) = &self.config.filename_template {
            properties.insert(
                "filename_template".to_string(),
                Value::String(filename_template.clone()),
            );
        }
        properties
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        log_component_start("File Reaction", &self.base.id);

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting file reaction".to_string()),
            )
            .await;

        self.base
            .set_status(
                ComponentStatus::Running,
                Some("File reaction started".to_string()),
            )
            .await;

        let priority_queue = self.base.priority_queue.clone();
        let reaction_name = self.base.id.clone();
        let config = self.config.clone();
        let handlebars = self.handlebars.clone();
        let file_locks = self.file_locks.clone();
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        let processing_task = tokio::spawn(async move {
            loop {
                let query_result_arc = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                        break;
                    }
                    result = priority_queue.dequeue() => result,
                };

                Self::process_result(
                    &config,
                    &handlebars,
                    &file_locks,
                    query_result_arc.as_ref(),
                    &reaction_name,
                )
                .await;
            }
        });

        self.base.set_processing_task(processing_task).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await?;
        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("File reaction stopped".to_string()),
            )
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{QueryConfig, TemplateSpec};
    use tempfile::TempDir;

    fn create_test_config() -> FileReactionConfig {
        FileReactionConfig {
            output_path: ".".to_string(),
            write_mode: WriteMode::Append,
            filename_template: Some("{{query_name}}_{{operation}}_{{after.id}}.log".to_string()),
            routes: HashMap::new(),
            default_template: None,
        }
    }

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(
            sanitize_filename(r#"bad/\:*?"<>|name"#),
            "bad_________name".to_string()
        );
    }

    #[test]
    fn test_sanitize_filename_reserved_windows_names() {
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("con"), "_con");
        assert_eq!(sanitize_filename("PRN"), "_PRN");
        assert_eq!(sanitize_filename("AUX"), "_AUX");
        assert_eq!(sanitize_filename("NUL"), "_NUL");
        assert_eq!(sanitize_filename("COM1"), "_COM1");
        assert_eq!(sanitize_filename("LPT9"), "_LPT9");
        // Reserved names with extensions are also invalid on Windows
        assert_eq!(sanitize_filename("CON.json"), "_CON.json");
        assert_eq!(sanitize_filename("nul.txt"), "_nul.txt");
        // Non-reserved names should pass through unchanged
        assert_eq!(sanitize_filename("CONSOLE"), "CONSOLE");
        assert_eq!(sanitize_filename("contact.json"), "contact.json");
    }

    #[test]
    fn test_sanitize_filename_control_chars() {
        assert_eq!(sanitize_filename("hello\x01world"), "hello_world");
    }

    #[test]
    fn test_invalid_template_fails_build() {
        let result = FileReaction::builder("test")
            .with_query("query1")
            .with_default_template(QueryConfig {
                added: Some(TemplateSpec::new("{{after.id")),
                ..Default::default()
            })
            .build();

        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_route_fails_build() {
        let result = FileReaction::builder("test")
            .with_query("query1")
            .with_route(
                "non-existent-query",
                QueryConfig {
                    added: Some(TemplateSpec::new("{{after.id}}")),
                    ..Default::default()
                },
            )
            .build();

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_payload_filename_template_and_sanitization() {
        let temp_dir = TempDir::new().expect("temp dir");
        let config = FileReactionConfig {
            output_path: temp_dir.path().to_string_lossy().to_string(),
            write_mode: WriteMode::PerChange,
            filename_template: Some("order_{{after.id}}_{{operation}}.json".to_string()),
            routes: HashMap::new(),
            default_template: Some(QueryConfig {
                added: Some(TemplateSpec::new(r#"{"id":{{after.id}}}"#)),
                ..Default::default()
            }),
        };

        let handlebars = FileReaction::build_handlebars();
        let file_locks = FileReaction::new_file_locks();
        let result = QueryResult::new(
            "orders".to_string(),
            chrono::Utc::now(),
            vec![ResultDiff::Add {
                data: serde_json::json!({"id": "a/b"}),
            }],
            HashMap::new(),
        );

        FileReaction::process_result(&config, &handlebars, &file_locks, &result, "test").await;

        let output_file = temp_dir.path().join("order_a_b_ADD.json");
        assert!(output_file.exists());
    }

    #[tokio::test]
    async fn test_fallback_to_raw_json_when_no_template() {
        let temp_dir = TempDir::new().expect("temp dir");
        let mut config = create_test_config();
        config.output_path = temp_dir.path().to_string_lossy().to_string();
        config.routes.clear();
        config.default_template = None;

        let handlebars = FileReaction::build_handlebars();
        let file_locks = FileReaction::new_file_locks();
        let result = QueryResult::new(
            "orders".to_string(),
            chrono::Utc::now(),
            vec![ResultDiff::Add {
                data: serde_json::json!({"id": 1}),
            }],
            HashMap::new(),
        );

        FileReaction::process_result(&config, &handlebars, &file_locks, &result, "test").await;

        let output_file = temp_dir.path().join("orders_ADD_1.log");
        let content = tokio::fs::read_to_string(output_file)
            .await
            .expect("read file");
        assert!(
            content.contains("\"ADD\"") && content.contains("\"id\""),
            "expected raw JSON diff payload, got: {content}"
        );
    }

    #[tokio::test]
    async fn test_default_template_route_lookup() {
        let temp_dir = TempDir::new().expect("temp dir");
        let config = FileReactionConfig {
            output_path: temp_dir.path().to_string_lossy().to_string(),
            write_mode: WriteMode::Append,
            filename_template: Some("{{query_name}}.log".to_string()),
            routes: HashMap::new(),
            default_template: Some(QueryConfig {
                added: Some(TemplateSpec::new("default-{{after.id}}")),
                ..Default::default()
            }),
        };

        let handlebars = FileReaction::build_handlebars();
        let file_locks = FileReaction::new_file_locks();
        let result = QueryResult::new(
            "unknown-query".to_string(),
            chrono::Utc::now(),
            vec![ResultDiff::Add {
                data: serde_json::json!({"id": 7}),
            }],
            HashMap::new(),
        );

        FileReaction::process_result(&config, &handlebars, &file_locks, &result, "test").await;

        let output_file = temp_dir.path().join("unknown-query.log");
        let content = tokio::fs::read_to_string(output_file)
            .await
            .expect("read file");
        assert!(
            content.contains("default-7"),
            "expected rendered template 'default-7', got: {content}"
        );
    }

    #[tokio::test]
    async fn test_query_route_overrides_default() {
        let temp_dir = TempDir::new().expect("temp dir");
        let mut routes = HashMap::new();
        routes.insert(
            "orders".to_string(),
            QueryConfig {
                added: Some(TemplateSpec::new("route-{{after.id}}")),
                ..Default::default()
            },
        );

        let config = FileReactionConfig {
            output_path: temp_dir.path().to_string_lossy().to_string(),
            write_mode: WriteMode::Append,
            filename_template: Some("{{query_name}}.log".to_string()),
            routes,
            default_template: Some(QueryConfig {
                added: Some(TemplateSpec::new("default-{{after.id}}")),
                ..Default::default()
            }),
        };

        let handlebars = FileReaction::build_handlebars();
        let file_locks = FileReaction::new_file_locks();
        let result = QueryResult::new(
            "orders".to_string(),
            chrono::Utc::now(),
            vec![ResultDiff::Add {
                data: serde_json::json!({"id": 9}),
            }],
            HashMap::new(),
        );

        FileReaction::process_result(&config, &handlebars, &file_locks, &result, "test").await;

        let output_file = temp_dir.path().join("orders.log");
        let content = tokio::fs::read_to_string(output_file)
            .await
            .expect("read file");
        assert!(
            content.contains("route-9"),
            "expected rendered template 'route-9', got: {content}"
        );
        assert!(
            !content.contains("default-9"),
            "should not contain default template output, got: {content}"
        );
    }
}
