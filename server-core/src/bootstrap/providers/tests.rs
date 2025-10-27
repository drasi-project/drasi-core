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

//! Unit tests for bootstrap providers

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::bootstrap::{
        BootstrapContext, BootstrapProvider, BootstrapProviderConfig, BootstrapRequest,
    };
    use crate::config::SourceConfig;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn create_test_context() -> (
        BootstrapContext,
        mpsc::Receiver<crate::channels::BootstrapEvent>,
        mpsc::Sender<crate::channels::BootstrapEvent>,
    ) {
        let (tx, rx) = mpsc::channel(100);

        let mut properties = HashMap::new();
        properties.insert("test_prop".to_string(), json!("test_value"));

        let source_config = Arc::new(SourceConfig {
            id: "test_source".to_string(),
            source_type: "test".to_string(),
            auto_start: true,
            properties,
            bootstrap_provider: None,
            broadcast_channel_capacity: None,
            dispatch_mode: None,
        });

        let context = BootstrapContext::new(
            "test_server".to_string(),
            source_config,
            "test_source".to_string(),
        );

        (context, rx, tx)
    }

    #[tokio::test]
    async fn test_noop_provider() {
        let provider = noop::NoOpBootstrapProvider::new();
        let (context, _rx, tx) = create_test_context();

        let request = BootstrapRequest {
            query_id: "test_query".to_string(),
            node_labels: vec!["Person".to_string()],
            relation_labels: vec![],
            request_id: "test_request".to_string(),
        };

        let result = provider.bootstrap(request, &context, tx).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_script_file_provider() {
        use std::path::PathBuf;

        // Use the person_small.jsonl fixture (3 Person nodes)
        let script_path = PathBuf::from("tests/fixtures/bootstrap_scripts/person_small.jsonl");
        let provider = script_file::ScriptFileBootstrapProvider::new(vec![script_path
            .to_string_lossy()
            .to_string()]);
        let (context, mut rx, tx) = create_test_context();

        let request = BootstrapRequest {
            query_id: "test_query".to_string(),
            node_labels: vec!["Person".to_string()],
            relation_labels: vec![],
            request_id: "test_request".to_string(),
        };

        let result = provider.bootstrap(request, &context, tx).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 3); // 3 Person nodes in person_small.jsonl

        // Verify that 3 events were sent
        for _i in 0..3 {
            let event = rx.recv().await.expect("Should receive event");
            assert_eq!(event.source_id, "test_source");
        }
    }

    #[tokio::test]
    async fn test_application_provider() {
        let provider = application::ApplicationBootstrapProvider::new();
        let (context, mut rx, tx) = create_test_context();

        // Test with empty bootstrap data
        let request = BootstrapRequest {
            query_id: "test_query".to_string(),
            node_labels: vec!["Person".to_string()],
            relation_labels: vec![],
            request_id: "test_request".to_string(),
        };

        let result = provider.bootstrap(request, &context, tx).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);

        // Verify no events were sent
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_bootstrap_provider_factory() {
        use crate::bootstrap::BootstrapProviderFactory;

        // Test NoOp provider creation
        let noop_config = BootstrapProviderConfig::Noop;
        let provider = BootstrapProviderFactory::create_provider(&noop_config);
        assert!(provider.is_ok());

        // Test ScriptFile provider creation
        let script_config = BootstrapProviderConfig::ScriptFile {
            file_paths: vec!["tests/fixtures/bootstrap_scripts/person_small.jsonl".to_string()],
        };
        let provider = BootstrapProviderFactory::create_provider(&script_config);
        assert!(provider.is_ok());
    }

    #[tokio::test]
    async fn test_bootstrap_context_properties() {
        let (context, _rx, _tx) = create_test_context();

        // Test getting a property that exists
        let prop = context.get_property("test_prop");
        assert!(prop.is_some());
        assert_eq!(prop.unwrap().as_str().unwrap(), "test_value");

        // Test getting a property that doesn't exist
        let missing = context.get_property("missing_prop");
        assert!(missing.is_none());

        // Test getting a typed property
        let typed_prop: Result<Option<String>, _> = context.get_typed_property("test_prop");
        assert!(typed_prop.is_ok());
        assert_eq!(typed_prop.unwrap().unwrap(), "test_value");
    }
}
