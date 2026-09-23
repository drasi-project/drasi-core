#[cfg(test)]
mod tests {
    use super::super::*;

    #[tokio::test]
    async fn test_descriptor_preserves_auto_start() {
        use crate::descriptor::MySqlSourceDescriptor;
        use drasi_plugin_sdk::prelude::SourcePluginDescriptor;

        let config = serde_json::json!({
            "database": "testdb",
            "user": "testuser"
        });
        for auto_start in [false, true] {
            let source = MySqlSourceDescriptor
                .create_source("test-source", &config, auto_start)
                .await
                .unwrap();

            assert_eq!(source.auto_start(), auto_start);
            assert_eq!(source.status().await, ComponentStatus::Stopped);
        }
    }

    #[test]
    fn test_builder_with_valid_config() {
        let source = MySqlReplicationSource::builder("test-source")
            .with_host("localhost")
            .with_database("testdb")
            .with_user("testuser")
            .with_password("testpass")
            .with_tables(vec!["users".to_string()])
            .build();
        assert!(source.is_ok());
    }

    #[test]
    fn test_config_validation_missing_database() {
        let config = MySqlSourceConfig {
            host: "localhost".to_string(),
            port: 3306,
            database: String::new(),
            user: "user".to_string(),
            password: String::new(),
            tables: vec![],
            ssl_mode: SslMode::Disabled,
            table_keys: vec![],
            start_position: StartPosition::FromEnd,
            server_id: 65535,
            heartbeat_interval_seconds: 30,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validation_missing_user() {
        let config = MySqlSourceConfig {
            host: "localhost".to_string(),
            port: 3306,
            database: "test".to_string(),
            user: String::new(),
            password: String::new(),
            tables: vec![],
            ssl_mode: SslMode::Disabled,
            table_keys: vec![],
            start_position: StartPosition::FromEnd,
            server_id: 65535,
            heartbeat_interval_seconds: 30,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validation_missing_host() {
        let config = MySqlSourceConfig {
            host: String::new(),
            port: 3306,
            database: "test".to_string(),
            user: "user".to_string(),
            password: String::new(),
            tables: vec![],
            ssl_mode: SslMode::Disabled,
            table_keys: vec![],
            start_position: StartPosition::FromEnd,
            server_id: 65535,
            heartbeat_interval_seconds: 30,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validation_rejects_zero_server_id() {
        let config = MySqlSourceConfig {
            host: "localhost".to_string(),
            port: 3306,
            database: "test".to_string(),
            user: "user".to_string(),
            password: String::new(),
            tables: vec![],
            ssl_mode: SslMode::Disabled,
            table_keys: vec![],
            start_position: StartPosition::FromEnd,
            server_id: 0,
            heartbeat_interval_seconds: 30,
        };
        assert!(config.validate().is_err());
    }

    mod lifecycle {
        use super::*;
        use bytes::Bytes;
        use drasi_lib::bootstrap::{BootstrapContext, BootstrapRequest, BootstrapResult};
        use drasi_lib::channels::BootstrapEventSender;
        use drasi_lib::config::SourceSubscriptionSettings;
        use std::time::Duration;

        struct InvalidBoundaryBootstrap;

        #[async_trait]
        impl BootstrapProvider for InvalidBoundaryBootstrap {
            async fn bootstrap(
                &self,
                _request: BootstrapRequest,
                _context: &BootstrapContext,
                _event_tx: BootstrapEventSender,
                _settings: Option<&SourceSubscriptionSettings>,
            ) -> Result<BootstrapResult> {
                Ok(BootstrapResult {
                    event_count: 0,
                    source_position: Some(Bytes::from_static(b"invalid boundary")),
                })
            }
        }

        #[tokio::test]
        async fn stop_cleans_up_after_replication_failure() {
            let source = MySqlSourceBuilder::new("failed-source")
                .with_host("127.0.0.1")
                .with_database("test")
                .with_user("test")
                .with_bootstrap_provider(InvalidBoundaryBootstrap)
                .build()
                .unwrap();

            for _ in 0..2 {
                source.start().await.unwrap();
                let mut subscription = source
                    .subscribe(SourceSubscriptionSettings {
                        source_id: source.id().to_string(),
                        query_id: "test-query".to_string(),
                        enable_bootstrap: true,
                        nodes: Default::default(),
                        relations: Default::default(),
                        resume_from: None,
                        resume_sequence: None,
                        request_position_handle: false,
                    })
                    .await
                    .unwrap();
                let mut status = source.base.status_handle().subscribe_status();
                tokio::time::timeout(
                    Duration::from_secs(3),
                    status.wait_for(|status| *status == ComponentStatus::Error),
                )
                .await
                .expect("invalid bootstrap boundary must fail the replication task")
                .unwrap();
                assert!(source.base.task_handle.read().await.is_some());

                source.stop().await.unwrap();
                assert_eq!(source.status().await, ComponentStatus::Stopped);
                assert!(source.base.task_handle.read().await.is_none());
                assert!(source.subscriber_resume_positions.read().await.is_empty());
                assert!(
                    tokio::time::timeout(Duration::from_secs(1), subscription.receiver.recv())
                        .await
                        .expect("stop must close the subscription")
                        .is_err()
                );
                source.stop().await.unwrap();
            }
        }

        #[tokio::test]
        async fn stop_joins_replication_task_before_returning() {
            let source = MySqlSourceBuilder::new("stopping-source")
                .with_database("test")
                .with_user("test")
                .build()
                .unwrap();
            for _ in 0..2 {
                source.start().await.unwrap();
                assert_eq!(source.status().await, ComponentStatus::Running);
                let task = source
                    .base
                    .task_handle
                    .read()
                    .await
                    .as_ref()
                    .unwrap()
                    .abort_handle();
                source.stop().await.unwrap();
                assert!(
                    task.is_finished(),
                    "stop must await the aborted replication task"
                );
                assert_eq!(source.status().await, ComponentStatus::Stopped);
                source.stop().await.unwrap();
            }
        }

        #[tokio::test]
        async fn stop_reports_unexpected_task_failure_after_cleanup() {
            let source = MySqlSourceBuilder::new("panicked-source")
                .with_database("test")
                .with_user("test")
                .build()
                .unwrap();
            source.start().await.unwrap();
            let failed = tokio::spawn(async { panic!("simulated replication task panic") });
            while !failed.is_finished() {
                tokio::task::yield_now().await;
            }
            let previous = source
                .base
                .task_handle
                .write()
                .await
                .replace(failed)
                .unwrap();
            previous.abort();
            assert!(previous.await.unwrap_err().is_cancelled());
            let mut receiver = source.base.create_streaming_receiver().await.unwrap();

            let error = source
                .stop()
                .await
                .expect_err("task panic must be surfaced");
            assert!(error
                .downcast_ref::<tokio::task::JoinError>()
                .unwrap()
                .is_panic());
            assert_eq!(source.status().await, ComponentStatus::Error);
            assert!(source.base.task_handle.read().await.is_none());
            assert!(
                tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                    .await
                    .unwrap()
                    .is_err()
            );
            source.stop().await.unwrap();
            assert_eq!(source.status().await, ComponentStatus::Stopped);
        }
    }

    mod position_comparator_tests {
        use bytes::Bytes;
        use drasi_lib::sources::PositionComparator;

        use crate::types::{MySqlPositionComparator, ReplicationState};

        fn make_position(file: &str, pos: u32, gtid: Option<&str>, ts: u64) -> Bytes {
            let state = ReplicationState {
                binlog_file: file.to_string(),
                binlog_position: pos,
                gtid_set: gtid.map(|s| s.to_string()),
                last_processed_timestamp: ts,
            };
            state.to_position_bytes()
        }

        #[test]
        fn test_same_file_higher_position_is_after() {
            let comparator = MySqlPositionComparator;
            let resume = make_position("mysql-bin.000001", 100, None, 1000);
            let event = make_position("mysql-bin.000001", 200, None, 1000);
            assert!(comparator.position_reached(&event, &resume));
        }

        #[test]
        fn test_same_file_lower_position_is_not_after() {
            let comparator = MySqlPositionComparator;
            let resume = make_position("mysql-bin.000001", 200, None, 1000);
            let event = make_position("mysql-bin.000001", 100, None, 1000);
            assert!(!comparator.position_reached(&event, &resume));
        }

        #[test]
        fn test_same_position_is_not_after() {
            let comparator = MySqlPositionComparator;
            let resume = make_position("mysql-bin.000001", 100, None, 1000);
            let event = make_position("mysql-bin.000001", 100, None, 1000);
            assert!(!comparator.position_reached(&event, &resume));
        }

        #[test]
        fn test_higher_timestamp_is_after() {
            let comparator = MySqlPositionComparator;
            let resume = make_position("mysql-bin.000001", 100, None, 1000);
            let event = make_position("mysql-bin.000001", 50, None, 2000);
            assert!(comparator.position_reached(&event, &resume));
        }

        #[test]
        fn test_lower_timestamp_is_not_after() {
            let comparator = MySqlPositionComparator;
            let resume = make_position("mysql-bin.000001", 100, None, 2000);
            let event = make_position("mysql-bin.000001", 200, None, 1000);
            assert!(!comparator.position_reached(&event, &resume));
        }

        #[test]
        fn test_different_file_same_timestamp() {
            let comparator = MySqlPositionComparator;
            let resume = make_position("mysql-bin.000001", 100, None, 1000);
            let event = make_position("mysql-bin.000002", 50, None, 1000);
            assert!(comparator.position_reached(&event, &resume));
        }

        #[test]
        fn test_gtid_positions_with_different_timestamps() {
            let comparator = MySqlPositionComparator;
            let resume = make_position("mysql-bin.000001", 100, Some("uuid1:1-5"), 1000);
            let event = make_position("mysql-bin.000001", 200, Some("uuid1:1-10"), 2000);
            assert!(comparator.position_reached(&event, &resume));
        }

        #[test]
        fn test_invalid_event_position_returns_false() {
            let comparator = MySqlPositionComparator;
            let resume = make_position("mysql-bin.000001", 100, None, 1000);
            let event = Bytes::from_static(b"not-valid-json");
            assert!(!comparator.position_reached(&event, &resume));
        }

        #[test]
        fn test_invalid_resume_position_returns_true() {
            let comparator = MySqlPositionComparator;
            let resume = Bytes::from_static(b"not-valid-json");
            let event = make_position("mysql-bin.000001", 100, None, 1000);
            assert!(comparator.position_reached(&event, &resume));
        }

        #[test]
        fn test_roundtrip_serialization() {
            let state = ReplicationState {
                binlog_file: "mysql-bin.000003".to_string(),
                binlog_position: 456,
                gtid_set: Some("abc-123:1-10".to_string()),
                last_processed_timestamp: 1700000000,
            };
            let bytes = state.to_position_bytes();
            let recovered = ReplicationState::from_position_bytes(&bytes).unwrap();
            assert_eq!(recovered.binlog_file, "mysql-bin.000003");
            assert_eq!(recovered.binlog_position, 456);
            assert_eq!(recovered.gtid_set, Some("abc-123:1-10".to_string()));
            assert_eq!(recovered.last_processed_timestamp, 1700000000);
        }
    }
}
