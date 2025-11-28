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
    use crate::test_support::helpers::test_mocks::create_test_mock_reaction;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    async fn create_test_manager() -> (Arc<ReactionManager>, mpsc::Receiver<ComponentEvent>, mpsc::Sender<ComponentEvent>) {
        let (event_tx, event_rx) = mpsc::channel(100);
        let manager = Arc::new(ReactionManager::new(event_tx.clone()));
        (manager, event_rx, event_tx)
    }

    #[tokio::test]
    async fn test_add_reaction() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        let reaction = create_test_mock_reaction(
            "test-reaction".to_string(),
            vec!["query1".to_string()],
            event_tx,
        );
        let result = manager.add_reaction(reaction).await;

        assert!(result.is_ok());

        // Verify reaction was added
        let reactions = manager.list_reactions().await;
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].0, "test-reaction");
    }

    #[tokio::test]
    async fn test_add_duplicate_reaction() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        let reaction1 = create_test_mock_reaction(
            "test-reaction".to_string(),
            vec![],
            event_tx.clone(),
        );
        let reaction2 = create_test_mock_reaction(
            "test-reaction".to_string(),
            vec![],
            event_tx,
        );

        // Add reaction first time
        assert!(manager.add_reaction(reaction1).await.is_ok());

        // Try to add same reaction again
        let result = manager.add_reaction(reaction2).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_delete_reaction() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        let reaction = create_test_mock_reaction(
            "test-reaction".to_string(),
            vec![],
            event_tx,
        );
        manager.add_reaction(reaction).await.unwrap();

        // Delete the reaction
        let result = manager.delete_reaction("test-reaction".to_string()).await;
        assert!(result.is_ok());

        // Verify reaction was removed
        let reactions = manager.list_reactions().await;
        assert_eq!(reactions.len(), 0);
    }

    #[tokio::test]
    async fn test_delete_nonexistent_reaction() {
        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        let result = manager.delete_reaction("nonexistent".to_string()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_get_reaction_info() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        let reaction = create_test_mock_reaction(
            "test-reaction".to_string(),
            vec!["query1".to_string()],
            event_tx,
        );
        manager.add_reaction(reaction).await.unwrap();

        let retrieved = manager.get_reaction("test-reaction".to_string()).await;
        assert!(retrieved.is_ok());

        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, "test-reaction");
        assert_eq!(retrieved.reaction_type, "log");
        assert_eq!(retrieved.queries, vec!["query1".to_string()]);
    }

    #[tokio::test]
    async fn test_list_reactions_with_status() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        // Add multiple reactions
        let reaction1 = create_test_mock_reaction(
            "reaction1".to_string(),
            vec![],
            event_tx.clone(),
        );
        let reaction2 = create_test_mock_reaction(
            "reaction2".to_string(),
            vec![],
            event_tx,
        );

        manager.add_reaction(reaction1).await.unwrap();
        manager.add_reaction(reaction2).await.unwrap();

        let reactions = manager.list_reactions().await;
        assert_eq!(reactions.len(), 2);

        // Both should be stopped initially
        for (_, status) in reactions {
            assert!(matches!(status, ComponentStatus::Stopped));
        }
    }

    #[tokio::test]
    async fn test_get_reaction_status() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        let reaction = create_test_mock_reaction(
            "test-reaction".to_string(),
            vec![],
            event_tx,
        );
        manager.add_reaction(reaction).await.unwrap();

        let status = manager.get_reaction_status("test-reaction".to_string()).await;
        assert!(status.is_ok());
        assert!(matches!(status.unwrap(), ComponentStatus::Stopped));
    }

    #[tokio::test]
    async fn test_get_nonexistent_reaction_status() {
        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        let status = manager.get_reaction_status("nonexistent".to_string()).await;
        assert!(status.is_err());
        assert!(status.unwrap_err().to_string().contains("not found"));
    }
}
