#[cfg(test)]
mod tests {
    use super::super::*;

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
        use drasi_lib::component_graph::{wait_for_status, ComponentUpdate};
        use drasi_lib::config::SourceSubscriptionSettings;
        use drasi_lib::DrasiLib;
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
        async fn managed_lifecycle_cleans_failed_source() {
            for action in ["stop", "stop_all", "remove", "update"] {
                let source = MySqlSourceBuilder::new("managed-source")
                    .with_database("test")
                    .with_user("test")
                    .with_bootstrap_provider(InvalidBoundaryBootstrap)
                    .build()
                    .unwrap();
                let core = DrasiLib::builder()
                    .with_source(source)
                    .build()
                    .await
                    .unwrap();
                core.start().await.unwrap();
                let graph = core.component_graph();
                let runtime = graph
                    .read()
                    .await
                    .get_runtime::<StdArc<dyn Source>>("managed-source")
                    .cloned()
                    .unwrap();
                let source = runtime
                    .as_any()
                    .downcast_ref::<MySqlReplicationSource>()
                    .unwrap();
                let mut subscription = source
                    .subscribe(SourceSubscriptionSettings {
                        source_id: source.id().into(),
                        query_id: "test-query".into(),
                        enable_bootstrap: true,
                        nodes: Default::default(),
                        relations: Default::default(),
                        resume_from: None,
                        resume_sequence: None,
                        request_position_handle: false,
                    })
                    .await
                    .unwrap();
                wait_for_status(
                    &graph,
                    source.id(),
                    &[ComponentStatus::Error],
                    Duration::from_secs(3),
                )
                .await
                .unwrap();
                let task = source
                    .base
                    .task_handle
                    .read()
                    .await
                    .as_ref()
                    .unwrap()
                    .abort_handle();

                match action {
                    "stop" => core.stop_source(source.id()).await.unwrap(),
                    "stop_all" => core.stop().await.unwrap(),
                    "remove" => core.remove_source(source.id(), false).await.unwrap(),
                    "update" => {
                        core.update_source(
                            source.id(),
                            MySqlSourceBuilder::new(source.id())
                                .with_database("test")
                                .with_user("test")
                                .build()
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                    _ => unreachable!(),
                }

                assert!(
                    task.is_finished(),
                    "{action} must join the replication task"
                );
                assert!(source.base.task_handle.read().await.is_none());
                assert!(source.subscriber_resume_positions.read().await.is_empty());
                assert_eq!(source.status().await, ComponentStatus::Stopped);
                assert!(
                    tokio::time::timeout(Duration::from_secs(1), subscription.receiver.recv())
                        .await
                        .unwrap()
                        .is_err()
                );
                if action == "remove" {
                    assert!(!graph.read().await.contains(source.id()));
                } else {
                    wait_for_status(
                        &graph,
                        source.id(),
                        &[ComponentStatus::Stopped],
                        Duration::from_secs(3),
                    )
                    .await
                    .unwrap();
                    assert!(
                        core.stop_source(source.id()).await.is_err(),
                        "managed repeated stop is rejected"
                    );
                }
                if action != "stop_all" {
                    core.stop().await.unwrap();
                }
            }
        }

        #[tokio::test]
        async fn stop_cleans_starting_source() {
            let source = MySqlSourceBuilder::new("starting-source")
                .with_database("test")
                .with_user("test")
                .build()
                .unwrap();
            source
                .base
                .set_status(ComponentStatus::Starting, None)
                .await;
            let task = tokio::spawn(std::future::pending::<()>());
            let abort_handle = task.abort_handle();
            source.base.set_task_handle(task).await;
            let mut receiver = source.base.create_streaming_receiver().await.unwrap();
            source.subscriber_resume_positions.write().await.insert(
                "old-query".into(),
                ReplicationState {
                    binlog_file: "binlog.000001".into(),
                    binlog_position: 4,
                    gtid_set: None,
                    last_processed_timestamp: 0,
                },
            );

            source.stop().await.unwrap();
            assert!(abort_handle.is_finished());
            assert!(source.base.task_handle.read().await.is_none());
            assert!(source.subscriber_resume_positions.read().await.is_empty());
            assert_eq!(source.status().await, ComponentStatus::Stopped);
            assert!(
                tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                    .await
                    .unwrap()
                    .is_err()
            );
            source.stop().await.unwrap();
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
            let (update_tx, mut updates) = tokio::sync::mpsc::channel(8);
            source.base.status_handle().wire(update_tx).await;

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
            let mut error_message = None;
            while let Ok(ComponentUpdate::Status {
                status, message, ..
            }) = updates.try_recv()
            {
                if status == ComponentStatus::Error {
                    error_message = message;
                }
            }
            assert_eq!(
                error_message.as_deref(),
                Some("MySQL replication task failed during shutdown; see logs for details")
            );
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
