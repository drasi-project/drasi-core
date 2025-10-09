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

use anyhow::Result;
use log::{error, info, warn};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::DrasiServerCoreConfig;
use crate::queries::QueryManager;
use crate::reactions::ReactionManager;
use crate::sources::SourceManager;

/// Manages configuration persistence for the Drasi server
#[derive(Clone)]
pub struct ConfigPersistence {
    config_path: PathBuf,
    server_settings: Arc<RwLock<crate::config::DrasiServerCoreSettings>>,
    source_manager: Arc<SourceManager>,
    query_manager: Arc<QueryManager>,
    reaction_manager: Arc<ReactionManager>,
    skip_save: bool, // Skip saving to disk if true (either read-only file or disable_persistence)
}

impl ConfigPersistence {
    pub fn new(
        config_path: PathBuf,
        server_settings: crate::config::DrasiServerCoreSettings,
        source_manager: Arc<SourceManager>,
        query_manager: Arc<QueryManager>,
        reaction_manager: Arc<ReactionManager>,
        skip_save: bool, // Skip saving to disk if true (either read-only file or disable_persistence)
    ) -> Self {
        Self {
            config_path,
            server_settings: Arc::new(RwLock::new(server_settings)),
            source_manager,
            query_manager,
            reaction_manager,
            skip_save,
        }
    }

    /// Save the current configuration to disk
    pub async fn save(&self) -> Result<()> {
        if self.skip_save {
            warn!("Skipping config save - either file is read-only or persistence is disabled");
            return Ok(());
        }

        // Build the complete configuration from all managers
        let config = self.build_current_config().await?;

        // Save to a temporary file first for atomicity
        let temp_path = self.config_path.with_extension("tmp");

        match config.save_to_file(&temp_path) {
            Ok(_) => {
                // Atomically rename the temp file to the actual config file
                match std::fs::rename(&temp_path, &self.config_path) {
                    Ok(_) => {
                        info!(
                            "Configuration saved successfully to: {}",
                            self.config_path.display()
                        );
                        Ok(())
                    }
                    Err(e) => {
                        error!("Failed to rename temp config file: {}", e);
                        // Try to clean up the temp file
                        let _ = std::fs::remove_file(&temp_path);
                        Err(anyhow::anyhow!("Failed to save configuration: {}", e))
                    }
                }
            }
            Err(e) => {
                error!("Failed to write configuration to temp file: {}", e);
                // Try to clean up the temp file if it exists
                let _ = std::fs::remove_file(&temp_path);
                Err(anyhow::anyhow!("Failed to save configuration: {}", e))
            }
        }
    }

    /// Build the current configuration from all managers
    async fn build_current_config(&self) -> Result<DrasiServerCoreConfig> {
        // Get all source configs
        let source_names = self.source_manager.list_sources().await;
        let mut sources = Vec::new();
        for (name, _) in source_names {
            if let Some(config) = self.source_manager.get_source_config(&name).await {
                sources.push(config);
            }
        }

        // Get all query configs
        let query_names = self.query_manager.list_queries().await;
        let mut queries = Vec::new();
        for (name, _) in query_names {
            if let Some(config) = self.query_manager.get_query_config(&name).await {
                queries.push(config);
            }
        }

        // Get all reaction configs
        let reaction_names = self.reaction_manager.list_reactions().await;
        let mut reactions = Vec::new();
        for (name, _) in reaction_names {
            if let Some(config) = self.reaction_manager.get_reaction_config(&name).await {
                reactions.push(config);
            }
        }

        // Sort by id for consistent ordering in the config file
        sources.sort_by(|a, b| a.id.cmp(&b.id));
        queries.sort_by(|a, b| a.id.cmp(&b.id));
        reactions.sort_by(|a, b| a.id.cmp(&b.id));

        Ok(DrasiServerCoreConfig {
            server: self.server_settings.read().await.clone(),
            sources,
            queries,
            reactions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_config_persistence_save() {
        // Create a temporary file for testing
        let temp_file = NamedTempFile::new().unwrap();
        let config_path = temp_file.path().to_path_buf();

        // Create test server settings
        let server_settings = crate::config::DrasiServerCoreSettings {
            host: "127.0.0.1".to_string(),
            port: 8080,
            log_level: "info".to_string(),
            max_connections: 1000,
            shutdown_timeout_seconds: 30,
            disable_persistence: false,
        };

        // Create managers (we'll use mock implementations for testing)
        let (channels, _receivers) = crate::channels::EventChannels::new();
        let source_manager = Arc::new(crate::sources::SourceManager::new(
            channels.source_change_tx.clone(),
            channels.component_event_tx.clone(),
        ));
        let query_manager = Arc::new(crate::queries::QueryManager::new(
            channels.query_result_tx.clone(),
            channels.component_event_tx.clone(),
            channels.bootstrap_request_tx.clone(),
        ));
        let reaction_manager = Arc::new(crate::reactions::ReactionManager::new(
            channels.component_event_tx.clone(),
        ));

        // Create config persistence
        let config_persistence = ConfigPersistence::new(
            config_path.clone(),
            server_settings.clone(),
            source_manager.clone(),
            query_manager.clone(),
            reaction_manager.clone(),
            false, // not skip_save (should save)
        );

        // Add a test source
        let source_config = crate::config::SourceConfig {
            id: "test-source".to_string(),
            source_type: "mock".to_string(),
            auto_start: true,
            properties: std::collections::HashMap::new(),
            bootstrap_provider: None,
        };
        source_manager
            .add_source(source_config.clone())
            .await
            .unwrap();

        // Save the configuration
        config_persistence.save().await.unwrap();

        // Load the saved configuration
        let loaded_config =
            crate::config::DrasiServerCoreConfig::load_from_file(&config_path).unwrap();

        // Verify the configuration was saved correctly
        assert_eq!(loaded_config.server.host, "127.0.0.1");
        assert_eq!(loaded_config.server.port, 8080);
        assert_eq!(loaded_config.sources.len(), 1);
        assert_eq!(loaded_config.sources[0].id, "test-source");
        assert_eq!(loaded_config.queries.len(), 0);
        assert_eq!(loaded_config.reactions.len(), 0);
    }

    #[tokio::test]
    async fn test_config_persistence_read_only() {
        // Create a temporary file for testing
        let temp_file = NamedTempFile::new().unwrap();
        let config_path = temp_file.path().to_path_buf();

        // Write initial config
        let initial_config = crate::config::DrasiServerCoreConfig::default();
        initial_config.save_to_file(&config_path).unwrap();

        // Create managers
        let (channels, _receivers) = crate::channels::EventChannels::new();
        let source_manager = Arc::new(crate::sources::SourceManager::new(
            channels.source_change_tx.clone(),
            channels.component_event_tx.clone(),
        ));
        let query_manager = Arc::new(crate::queries::QueryManager::new(
            channels.query_result_tx.clone(),
            channels.component_event_tx.clone(),
            channels.bootstrap_request_tx.clone(),
        ));
        let reaction_manager = Arc::new(crate::reactions::ReactionManager::new(
            channels.component_event_tx.clone(),
        ));

        // Create config persistence in skip_save mode
        let config_persistence = ConfigPersistence::new(
            config_path.clone(),
            crate::config::DrasiServerCoreSettings::default(),
            source_manager.clone(),
            query_manager.clone(),
            reaction_manager.clone(),
            true, // skip_save (don't write to disk)
        );

        // Add a test source
        let source_config = crate::config::SourceConfig {
            id: "test-source".to_string(),
            source_type: "mock".to_string(),
            auto_start: true,
            properties: std::collections::HashMap::new(),
            bootstrap_provider: None,
        };
        source_manager
            .add_source(source_config.clone())
            .await
            .unwrap();

        // Try to save the configuration (should succeed but not write)
        config_persistence.save().await.unwrap();

        // Load the saved configuration - should still be empty
        let loaded_config =
            crate::config::DrasiServerCoreConfig::load_from_file(&config_path).unwrap();
        assert_eq!(loaded_config.sources.len(), 0); // Should still be empty
    }
}
