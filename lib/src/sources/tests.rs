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
mod manager_tests {
    use super::super::*;
    use crate::channels::*;
    use crate::test_support::helpers::test_fixtures::*;
    use crate::test_support::helpers::test_mocks::create_test_source_registry;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    async fn create_test_manager() -> (Arc<SourceManager>, mpsc::Receiver<ComponentEvent>) {
        let (event_tx, event_rx) = mpsc::channel(100);
        let registry = create_test_source_registry();
        let manager = Arc::new(SourceManager::with_registry(event_tx, registry));
        (manager, event_rx)
    }

    #[tokio::test]
    async fn test_add_source() {
        let (manager, _event_rx) = create_test_manager().await;

        let config = create_test_source_config("test-source", "mock");
        let result = manager.add_source(config.clone()).await;

        assert!(result.is_ok());

        // Verify source was added
        let sources = manager.list_sources().await;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].0, "test-source");
    }

    #[tokio::test]
    async fn test_add_duplicate_source() {
        let (manager, _event_rx) = create_test_manager().await;

        let config = create_test_source_config("test-source", "mock");

        // Add source first time
        assert!(manager.add_source(config.clone()).await.is_ok());

        // Try to add same source again
        let result = manager.add_source(config).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_remove_source() {
        let (manager, _event_rx) = create_test_manager().await;

        let config = create_test_source_config("test-source", "mock");
        manager.add_source(config).await.unwrap();

        // Remove the source
        let result = manager.delete_source("test-source".to_string()).await;
        assert!(result.is_ok());

        // Verify source was removed
        let sources = manager.list_sources().await;
        assert_eq!(sources.len(), 0);
    }

    #[tokio::test]
    async fn test_remove_nonexistent_source() {
        let (manager, _event_rx) = create_test_manager().await;

        let result = manager.delete_source("nonexistent".to_string()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_start_source() {
        let (manager, mut event_rx) = create_test_manager().await;

        let mut config = create_test_source_config("test-source", "mock");
        config.auto_start = false;
        manager.add_source(config).await.unwrap();

        // Start the source
        let result = manager.start_source("test-source".to_string()).await;
        assert!(result.is_ok());

        // Check for status event
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Some(event) = event_rx.recv().await {
                if event.component_id == "test-source" {
                    assert!(
                        matches!(event.status, ComponentStatus::Starting)
                            || matches!(event.status, ComponentStatus::Running)
                    );
                    break;
                }
            }
        })
        .await
        .expect("Timeout waiting for status event");
    }

    #[tokio::test]
    async fn test_stop_source() {
        let (manager, mut event_rx) = create_test_manager().await;

        let mut config = create_test_source_config("test-source", "mock");
        config.auto_start = false;
        manager.add_source(config).await.unwrap();
        manager
            .start_source("test-source".to_string())
            .await
            .unwrap();

        // Wait a bit for source to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Stop the source
        let result = manager.stop_source("test-source".to_string()).await;
        assert!(result.is_ok());

        // Check for stop event
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Some(event) = event_rx.recv().await {
                if event.component_id == "test-source"
                    && matches!(event.status, ComponentStatus::Stopped)
                {
                    break;
                }
            }
        })
        .await
        .expect("Timeout waiting for stop event");
    }

    #[tokio::test]
    async fn test_get_source_config() {
        let (manager, _event_rx) = create_test_manager().await;

        let config = create_test_source_config("test-source", "mock");
        manager.add_source(config.clone()).await.unwrap();

        let retrieved = manager.get_source_config("test-source").await;
        assert!(retrieved.is_some());

        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, config.id);
        assert_eq!(retrieved.source_type(), config.source_type());
    }

    #[tokio::test]
    async fn test_update_source() {
        let (manager, _event_rx) = create_test_manager().await;

        let mut config = create_test_source_config("test-source", "mock");
        config.auto_start = false;
        manager.add_source(config.clone()).await.unwrap();

        // Update config by modifying the typed config
        // Since we have strongly-typed configs, we can't just insert a new property
        // Instead, verify the config was updated correctly
        let result = manager
            .update_source("test-source".to_string(), config.clone())
            .await;
        assert!(result.is_ok());

        // Verify update
        let retrieved = manager.get_source_config("test-source").await.unwrap();
        assert_eq!(retrieved.id, "test-source");
    }

    #[tokio::test]
    async fn test_list_sources_with_status() {
        let (manager, _event_rx) = create_test_manager().await;

        // Add multiple sources
        let mut config1 = create_test_source_config("source1", "mock");
        let mut config2 = create_test_source_config("source2", "mock");
        config1.auto_start = false;
        config2.auto_start = false;

        manager.add_source(config1).await.unwrap();
        manager.add_source(config2).await.unwrap();

        // Start one source
        manager.start_source("source1".to_string()).await.unwrap();

        // Wait a bit
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let sources = manager.list_sources().await;
        assert_eq!(sources.len(), 2);

        // Check that we have different statuses
        let source1_status = sources
            .iter()
            .find(|(name, _)| name == "source1")
            .unwrap()
            .1
            .clone();
        let source2_status = sources
            .iter()
            .find(|(name, _)| name == "source2")
            .unwrap()
            .1
            .clone();

        assert!(matches!(source1_status, ComponentStatus::Running));
        assert!(matches!(source2_status, ComponentStatus::Stopped));
    }
}
