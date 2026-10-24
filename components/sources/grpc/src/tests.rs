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

//! Unit tests for gRPC source plugin.

use super::*;

async fn restored_log_waits_for_the_slow_subscriber() -> anyhow::Result<()> {
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };
    use drasi_lib::{DurabilityConfig, WalProvider};
    use std::sync::atomic::Ordering;

    let directory = tempfile::tempdir()?;
    let id = "startup-wal";
    let durability = DurabilityConfig {
        enabled: true,
        ..Default::default()
    };
    {
        let seed = drasi_wal_redb::RedbWalProvider::new(directory.path());
        seed.register(id, durability.to_wal_config()).await?;
        for sequence in 1..=100 {
            seed.append(
                id,
                &SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new(id, &sequence.to_string()),
                            labels: Arc::from([Arc::from("Item")]),
                            effective_from: sequence,
                        },
                        properties: ElementPropertyMap::new(),
                    },
                },
            )
            .await?;
        }
    }
    let wal = Arc::new(drasi_wal_redb::RedbWalProvider::new(directory.path()));
    let source = GrpcSource::new(
        id,
        GrpcSourceConfig {
            host: "127.0.0.1".into(),
            port: 0,
            durability: Some(durability),
            ..Default::default()
        },
    )?;
    let (updates, _observer) = tokio::sync::mpsc::channel(16);
    let mut context = drasi_lib::SourceRuntimeContext::new("fixture", id, None, updates, None);
    context.wal_provider = Some(wal.clone());
    source.initialize(context).await;
    source.start().await?;
    let settings = |query: &str, sequence| drasi_lib::config::SourceSubscriptionSettings {
        source_id: id.into(),
        query_id: query.into(),
        nodes: Default::default(),
        relations: Default::default(),
        enable_bootstrap: false,
        resume_from: None,
        resume_sequence: Some(sequence),
        request_position_handle: true,
    };
    let fast = source.subscribe(settings("fast", 100)).await?;
    source.base.prune_confirmed_wal(wal.as_ref()).await?;
    assert_eq!(
        wal.event_count(id).await?,
        100,
        "unregistered recovery consumer was pruned"
    );
    let mut slow = source.subscribe(settings("slow", 80)).await?;
    source.on_subscriptions_complete().await;
    source.base.prune_confirmed_wal(wal.as_ref()).await?;
    assert_eq!(wal.event_count(id).await?, 20);
    for sequence in 81..=100 {
        let event = tokio::time::timeout(Duration::from_secs(2), slow.receiver.recv()).await??;
        assert_eq!(event.sequence, sequence);
        assert_eq!(event.source_id, id);
    }
    slow.position_handle
        .as_ref()
        .unwrap()
        .store(100, Ordering::Release);
    source.base.prune_confirmed_wal(wal.as_ref()).await?;
    assert_eq!(wal.event_count(id).await?, 0);
    assert!(matches!(
        wal.read_from(id, 81).await,
        Err(drasi_lib::WalError::PositionUnavailable {
            oldest_available: None,
            ..
        })
    ));
    drop((fast, slow));
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn wal_restart_preserves_late_subscriber_current_thread() -> anyhow::Result<()> {
    restored_log_waits_for_the_slow_subscriber().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wal_restart_preserves_late_subscriber_multiple_threads() -> anyhow::Result<()> {
    restored_log_waits_for_the_slow_subscriber().await
}

#[tokio::test]
async fn stop_joins_the_wal_pruner() {
    let source = GrpcSource::new("pruner", GrpcSourceConfig::default()).unwrap();
    let (started, ready) = tokio::sync::oneshot::channel();
    let mut stopping = source.base.subscribe_wal_pruning_stop();
    let task = tokio::spawn(async move {
        started.send(()).unwrap();
        stopping.wait_for(|stopping| *stopping).await.unwrap();
    });
    let worker = task.abort_handle();
    *source.prune_task.write().await = Some(task);
    ready.await.unwrap();
    source.stop().await.unwrap();
    assert!(worker.is_finished());
    assert!(source.prune_task.read().await.is_none());
}

#[tokio::test]
async fn cancelled_stop_retains_submitted_pruning_and_rejects_restart() {
    let source = GrpcSource::new("pruner", GrpcSourceConfig::default()).unwrap();
    let (submitted, submission) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    *source.prune_task.write().await = Some(tokio::spawn({
        let completed = completed.clone();
        async move {
            submitted.send(()).unwrap();
            released.await.unwrap();
            completed.store(true, std::sync::atomic::Ordering::Release);
        }
    }));
    submission.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), source.stop())
            .await
            .is_err()
    );
    assert!(source.prune_task.read().await.is_some());
    assert!(source.start().await.is_err());
    release.send(()).unwrap();
    source.stop().await.unwrap();
    assert!(completed.load(std::sync::atomic::Ordering::Acquire));
    assert!(source.prune_task.read().await.is_none());
}

mod construction {
    use super::*;

    #[test]
    fn test_new_with_valid_config() {
        let config = GrpcSourceConfig::default();
        let source = GrpcSource::new("test-source", config);
        assert!(source.is_ok());
    }

    #[test]
    fn test_new_with_custom_config() {
        let config = GrpcSourceConfig {
            host: "127.0.0.1".to_string(),
            port: 50052,
            endpoint: Some("/custom".to_string()),
            timeout_ms: 10000,
            durability: None,
        };
        let source = GrpcSource::new("custom-source", config).unwrap();
        assert_eq!(source.id(), "custom-source");
    }

    #[test]
    fn test_with_dispatch_creates_source() {
        let config = GrpcSourceConfig::default();
        let source = GrpcSource::with_dispatch(
            "dispatch-source",
            config,
            Some(DispatchMode::Channel),
            Some(1000),
        );
        assert!(source.is_ok());
        assert_eq!(source.unwrap().id(), "dispatch-source");
    }
}

mod properties {
    use super::*;

    #[test]
    fn test_id_returns_correct_value() {
        let config = GrpcSourceConfig::default();
        let source = GrpcSource::new("my-grpc-source", config).unwrap();
        assert_eq!(source.id(), "my-grpc-source");
    }

    #[test]
    fn test_type_name_returns_grpc() {
        let config = GrpcSourceConfig::default();
        let source = GrpcSource::new("test", config).unwrap();
        assert_eq!(source.type_name(), "grpc");
    }

    #[test]
    fn test_properties_contains_host_and_port() {
        let config = GrpcSourceConfig {
            host: "192.168.1.1".to_string(),
            port: 9000,
            endpoint: None,
            timeout_ms: 5000,
            durability: None,
        };
        let source = GrpcSource::new("test", config).unwrap();
        let props = source.properties();

        assert_eq!(
            props.get("host"),
            Some(&serde_json::Value::String("192.168.1.1".to_string()))
        );
        assert_eq!(
            props.get("port"),
            Some(&serde_json::Value::Number(9000.into()))
        );
    }

    #[test]
    fn test_properties_includes_endpoint_when_set() {
        let config = GrpcSourceConfig {
            host: "localhost".to_string(),
            port: 50051,
            endpoint: Some("/api/v1".to_string()),
            timeout_ms: 5000,
            durability: None,
        };
        let source = GrpcSource::new("test", config).unwrap();
        let props = source.properties();

        assert_eq!(
            props.get("endpoint"),
            Some(&serde_json::Value::String("/api/v1".to_string()))
        );
    }

    #[test]
    fn test_properties_excludes_endpoint_when_none() {
        let config = GrpcSourceConfig {
            host: "localhost".to_string(),
            port: 50051,
            endpoint: None,
            timeout_ms: 5000,
            durability: None,
        };
        let source = GrpcSource::new("test", config).unwrap();
        let props = source.properties();

        assert!(!props.contains_key("endpoint"));
    }
}

mod lifecycle {
    use super::*;

    #[tokio::test]
    async fn test_initial_status_is_stopped() {
        let config = GrpcSourceConfig::default();
        let source = GrpcSource::new("test", config).unwrap();
        assert_eq!(source.status().await, ComponentStatus::Stopped);
    }
}

mod builder {
    use super::*;

    #[test]
    fn test_builder_defaults() {
        let source = GrpcSourceBuilder::new("test-grpc").build().unwrap();
        assert_eq!(source.config.host, "0.0.0.0");
        assert_eq!(source.config.port, 50051);
        assert_eq!(source.config.endpoint, None);
        assert_eq!(source.config.timeout_ms, 5000);
    }

    #[test]
    fn test_builder_with_host() {
        let source = GrpcSourceBuilder::new("test-grpc")
            .with_host("192.168.1.100")
            .build()
            .unwrap();
        assert_eq!(source.config.host, "192.168.1.100");
    }

    #[test]
    fn test_builder_with_port() {
        let source = GrpcSourceBuilder::new("test-grpc")
            .with_port(9999)
            .build()
            .unwrap();
        assert_eq!(source.config.port, 9999);
    }

    #[test]
    fn test_builder_chaining() {
        let source = GrpcSourceBuilder::new("test-grpc")
            .with_host("grpc.example.com")
            .with_port(443)
            .build()
            .unwrap();

        assert_eq!(source.config.host, "grpc.example.com");
        assert_eq!(source.config.port, 443);
    }

    #[test]
    fn test_builder_id() {
        let source = GrpcSource::builder("my-grpc-source")
            .with_host("localhost")
            .build()
            .unwrap();

        assert_eq!(source.base.id, "my-grpc-source");
    }
}

mod config {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = GrpcSourceConfig::default();
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 50051);
        assert_eq!(config.timeout_ms, 5000);
        assert!(config.endpoint.is_none());
    }

    #[test]
    fn test_config_serialization() {
        let config = GrpcSourceConfig {
            host: "localhost".to_string(),
            port: 50051,
            endpoint: Some("/api".to_string()),
            timeout_ms: 10000,
            durability: None,
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: GrpcSourceConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_config_deserialization_with_defaults() {
        let json = r#"{"host": "127.0.0.1", "port": 8080}"#;
        let config: GrpcSourceConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 8080);
        assert_eq!(config.timeout_ms, 5000); // default
        assert!(config.endpoint.is_none());
    }

    #[test]
    fn test_config_deserialization_empty_uses_defaults() {
        let json = "{}";
        let config: GrpcSourceConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 50051);
        assert_eq!(config.timeout_ms, 5000);
    }
}

mod proto_conversion {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn test_proto_value_to_json_null() {
        let value = prost_types::Value {
            kind: Some(prost_types::value::Kind::NullValue(0)),
        };
        let result = proto_value_to_json(&value);
        assert_eq!(result, serde_json::Value::Null);
    }

    #[test]
    fn test_proto_value_to_json_bool() {
        let value = prost_types::Value {
            kind: Some(prost_types::value::Kind::BoolValue(true)),
        };
        let result = proto_value_to_json(&value);
        assert_eq!(result, serde_json::Value::Bool(true));
    }

    #[test]
    fn test_proto_value_to_json_number() {
        let value = prost_types::Value {
            kind: Some(prost_types::value::Kind::NumberValue(42.5)),
        };
        let result = proto_value_to_json(&value);
        assert_eq!(result, serde_json::json!(42.5));
    }

    #[test]
    fn test_proto_value_to_json_string() {
        let value = prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue("hello".to_string())),
        };
        let result = proto_value_to_json(&value);
        assert_eq!(result, serde_json::Value::String("hello".to_string()));
    }

    #[test]
    fn test_proto_value_to_json_list() {
        let list = prost_types::ListValue {
            values: vec![
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::NumberValue(1.0)),
                },
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::NumberValue(2.0)),
                },
            ],
        };
        let value = prost_types::Value {
            kind: Some(prost_types::value::Kind::ListValue(list)),
        };
        let result = proto_value_to_json(&value);
        assert_eq!(result, serde_json::json!([1.0, 2.0]));
    }

    #[test]
    fn test_proto_value_to_json_struct() {
        let mut fields = BTreeMap::new();
        fields.insert(
            "key".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue("value".to_string())),
            },
        );
        let struct_val = prost_types::Struct { fields };
        let value = prost_types::Value {
            kind: Some(prost_types::value::Kind::StructValue(struct_val)),
        };
        let result = proto_value_to_json(&value);
        assert_eq!(result, serde_json::json!({"key": "value"}));
    }

    #[test]
    fn test_proto_value_to_json_none() {
        let value = prost_types::Value { kind: None };
        let result = proto_value_to_json(&value);
        assert_eq!(result, serde_json::Value::Null);
    }

    #[test]
    fn test_convert_proto_value_to_element_value_null() {
        let value = prost_types::Value {
            kind: Some(prost_types::value::Kind::NullValue(0)),
        };
        let result = convert_proto_value_to_element_value(&value).unwrap();
        assert!(matches!(result, drasi_core::models::ElementValue::Null));
    }

    #[test]
    fn test_convert_proto_value_to_element_value_bool() {
        let value = prost_types::Value {
            kind: Some(prost_types::value::Kind::BoolValue(false)),
        };
        let result = convert_proto_value_to_element_value(&value).unwrap();
        assert!(matches!(
            result,
            drasi_core::models::ElementValue::Bool(false)
        ));
    }

    #[test]
    fn test_convert_proto_value_to_element_value_integer() {
        let value = prost_types::Value {
            kind: Some(prost_types::value::Kind::NumberValue(42.0)),
        };
        let result = convert_proto_value_to_element_value(&value).unwrap();
        assert!(matches!(
            result,
            drasi_core::models::ElementValue::Integer(42)
        ));
    }

    #[test]
    fn test_convert_proto_value_to_element_value_float() {
        let value = prost_types::Value {
            kind: Some(prost_types::value::Kind::NumberValue(1.23456)),
        };
        let result = convert_proto_value_to_element_value(&value).unwrap();
        match result {
            drasi_core::models::ElementValue::Float(f) => {
                assert!((f.0 - 1.23456).abs() < 0.001);
            }
            _ => panic!("Expected Float"),
        }
    }

    #[test]
    fn test_convert_proto_value_to_element_value_string() {
        let value = prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue("test".to_string())),
        };
        let result = convert_proto_value_to_element_value(&value).unwrap();
        match result {
            drasi_core::models::ElementValue::String(s) => {
                assert_eq!(s.as_ref(), "test");
            }
            _ => panic!("Expected String"),
        }
    }

    /// Validates that gRPC source converts nanosecond wire timestamps to
    /// milliseconds when constructing ElementMetadata.
    #[test]
    fn test_proto_metadata_converts_nanos_to_millis() {
        use drasi_core::models::validate_effective_from;

        // Simulate a nanosecond timestamp from protobuf (Feb 2026)
        let nanos: u64 = 1_771_000_000_000_000_000;
        let proto_meta = proto::ElementMetadata {
            reference: Some(proto::ElementReference {
                source_id: "src".to_string(),
                element_id: "e1".to_string(),
            }),
            labels: vec!["Test".to_string()],
            effective_from: nanos,
        };

        let core_meta = convert_proto_metadata_to_core(&proto_meta, "test-src").unwrap();
        assert!(
            validate_effective_from(core_meta.effective_from).is_ok(),
            "gRPC effective_from ({}) should be in millisecond range after ns->ms conversion",
            core_meta.effective_from
        );
    }
}
