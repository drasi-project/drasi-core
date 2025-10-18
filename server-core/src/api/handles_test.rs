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

//! Tests for HandleRegistry

#[cfg(test)]
mod tests {
    use crate::api::HandleRegistry;
    use crate::test_support::helpers::create_test_application_reaction;
    use crate::test_support::helpers::create_test_application_source;

    #[tokio::test]
    async fn test_registry_new() {
        let registry = HandleRegistry::new();
        assert!(!registry.has_source_handle("any-id").await);
        assert!(!registry.has_reaction_handle("any-id").await);
    }

    #[tokio::test]
    async fn test_registry_default() {
        let registry = HandleRegistry::default();
        assert!(!registry.has_source_handle("any-id").await);
        assert!(!registry.has_reaction_handle("any-id").await);
    }

    #[tokio::test]
    async fn test_register_and_get_source_handle() {
        let registry = HandleRegistry::new();
        let (_source, handle) = create_test_application_source("test-source").await;

        registry
            .register_source_handle("test-source".to_string(), handle.clone())
            .await;

        let retrieved = registry.get_source_handle("test-source").await;
        assert!(
            retrieved.is_ok(),
            "Should retrieve registered source handle"
        );
    }

    #[tokio::test]
    async fn test_register_and_get_reaction_handle() {
        let registry = HandleRegistry::new();
        let (_reaction, handle) = create_test_application_reaction("test-reaction").await;

        registry
            .register_reaction_handle("test-reaction".to_string(), handle.clone())
            .await;

        let retrieved = registry.get_reaction_handle("test-reaction").await;
        assert!(
            retrieved.is_ok(),
            "Should retrieve registered reaction handle"
        );
    }

    #[tokio::test]
    async fn test_has_source_handle() {
        let registry = HandleRegistry::new();
        let (_source, handle) = create_test_application_source("test-source").await;

        assert!(
            !registry.has_source_handle("test-source").await,
            "Should not have handle before registration"
        );

        registry
            .register_source_handle("test-source".to_string(), handle)
            .await;

        assert!(
            registry.has_source_handle("test-source").await,
            "Should have handle after registration"
        );
    }

    #[tokio::test]
    async fn test_has_reaction_handle() {
        let registry = HandleRegistry::new();
        let (_reaction, handle) = create_test_application_reaction("test-reaction").await;

        assert!(
            !registry.has_reaction_handle("test-reaction").await,
            "Should not have handle before registration"
        );

        registry
            .register_reaction_handle("test-reaction".to_string(), handle)
            .await;

        assert!(
            registry.has_reaction_handle("test-reaction").await,
            "Should have handle after registration"
        );
    }

    #[tokio::test]
    async fn test_get_nonexistent_source_handle() {
        let registry = HandleRegistry::new();

        let result = registry.get_source_handle("nonexistent").await;
        assert!(
            result.is_err(),
            "Should return error for nonexistent source"
        );
    }

    #[tokio::test]
    async fn test_get_nonexistent_reaction_handle() {
        let registry = HandleRegistry::new();

        let result = registry.get_reaction_handle("nonexistent").await;
        assert!(
            result.is_err(),
            "Should return error for nonexistent reaction"
        );
    }

    #[tokio::test]
    async fn test_remove_source_handle() {
        let registry = HandleRegistry::new();
        let (_source, handle) = create_test_application_source("test-source").await;

        registry
            .register_source_handle("test-source".to_string(), handle)
            .await;
        assert!(registry.has_source_handle("test-source").await);

        let removed = registry.remove_source_handle("test-source").await;
        assert!(removed.is_some(), "Should return removed handle");
        assert!(
            !registry.has_source_handle("test-source").await,
            "Should not have handle after removal"
        );
    }

    #[tokio::test]
    async fn test_remove_reaction_handle() {
        let registry = HandleRegistry::new();
        let (_reaction, handle) = create_test_application_reaction("test-reaction").await;

        registry
            .register_reaction_handle("test-reaction".to_string(), handle)
            .await;
        assert!(registry.has_reaction_handle("test-reaction").await);

        let removed = registry.remove_reaction_handle("test-reaction").await;
        assert!(removed.is_some(), "Should return removed handle");
        assert!(
            !registry.has_reaction_handle("test-reaction").await,
            "Should not have handle after removal"
        );
    }

    #[tokio::test]
    async fn test_remove_nonexistent_source() {
        let registry = HandleRegistry::new();
        let removed = registry.remove_source_handle("nonexistent").await;
        assert!(
            removed.is_none(),
            "Should return None for nonexistent source"
        );
    }

    #[tokio::test]
    async fn test_remove_nonexistent_reaction() {
        let registry = HandleRegistry::new();
        let removed = registry.remove_reaction_handle("nonexistent").await;
        assert!(
            removed.is_none(),
            "Should return None for nonexistent reaction"
        );
    }

    #[tokio::test]
    async fn test_overwrite_source_handle() {
        let registry = HandleRegistry::new();
        let (_source1, handle1) = create_test_application_source("test-source").await;
        let (_source2, handle2) = create_test_application_source("test-source").await;

        registry
            .register_source_handle("test-source".to_string(), handle1)
            .await;
        registry
            .register_source_handle("test-source".to_string(), handle2)
            .await;

        assert!(
            registry.has_source_handle("test-source").await,
            "Should still have handle after overwrite"
        );
    }

    #[tokio::test]
    async fn test_overwrite_reaction_handle() {
        let registry = HandleRegistry::new();
        let (_reaction1, handle1) = create_test_application_reaction("test-reaction").await;
        let (_reaction2, handle2) = create_test_application_reaction("test-reaction").await;

        registry
            .register_reaction_handle("test-reaction".to_string(), handle1)
            .await;
        registry
            .register_reaction_handle("test-reaction".to_string(), handle2)
            .await;

        assert!(
            registry.has_reaction_handle("test-reaction").await,
            "Should still have handle after overwrite"
        );
    }

    #[tokio::test]
    async fn test_registry_clone() {
        let registry1 = HandleRegistry::new();
        let (_source, handle) = create_test_application_source("test-source").await;

        registry1
            .register_source_handle("test-source".to_string(), handle)
            .await;

        let registry2 = registry1.clone();
        assert!(
            registry2.has_source_handle("test-source").await,
            "Cloned registry should have same handles"
        );
    }

    #[tokio::test]
    async fn test_concurrent_source_registration() {
        let registry = HandleRegistry::new();
        let registry_clone = registry.clone();

        let task1 = tokio::spawn(async move {
            let (_source, handle) = create_test_application_source("source-1").await;
            registry
                .register_source_handle("source-1".to_string(), handle)
                .await;
        });

        let task2 = tokio::spawn(async move {
            let (_source, handle) = create_test_application_source("source-2").await;
            registry_clone
                .register_source_handle("source-2".to_string(), handle)
                .await;
        });

        let _ = tokio::join!(task1, task2);
    }

    #[tokio::test]
    async fn test_concurrent_reaction_registration() {
        let registry = HandleRegistry::new();
        let registry_clone = registry.clone();

        let task1 = tokio::spawn(async move {
            let (_reaction, handle) = create_test_application_reaction("reaction-1").await;
            registry
                .register_reaction_handle("reaction-1".to_string(), handle)
                .await;
        });

        let task2 = tokio::spawn(async move {
            let (_reaction, handle) = create_test_application_reaction("reaction-2").await;
            registry_clone
                .register_reaction_handle("reaction-2".to_string(), handle)
                .await;
        });

        let _ = tokio::join!(task1, task2);
    }

    #[tokio::test]
    async fn test_multiple_sources_and_reactions() {
        let registry = HandleRegistry::new();

        // Register multiple sources
        for i in 1..=5 {
            let (_source, handle) = create_test_application_source(&format!("source-{}", i)).await;
            registry
                .register_source_handle(format!("source-{}", i), handle)
                .await;
        }

        // Register multiple reactions
        for i in 1..=5 {
            let (_reaction, handle) =
                create_test_application_reaction(&format!("reaction-{}", i)).await;
            registry
                .register_reaction_handle(format!("reaction-{}", i), handle)
                .await;
        }

        // Verify all are present
        for i in 1..=5 {
            assert!(registry.has_source_handle(&format!("source-{}", i)).await);
            assert!(
                registry
                    .has_reaction_handle(&format!("reaction-{}", i))
                    .await
            );
        }
    }
}
