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

    #[tokio::test]
    async fn test_subscribe_rejects_incomplete_row_position() {
        let source = MySqlSourceBuilder::new("mysql-invalid-resume")
            .with_database("test")
            .with_user("test")
            .build()
            .unwrap();
        let token = serde_json::json!({
            "binlog_file": "mysql-bin.000001",
            "binlog_position": 200,
            "gtid_set": null,
            "last_processed_timestamp": 100,
            "row_offset": 1
        });
        let result = source
            .subscribe(drasi_lib::config::SourceSubscriptionSettings {
                source_id: source.id().to_string(),
                query_id: "query".to_string(),
                enable_bootstrap: false,
                nodes: Default::default(),
                relations: Default::default(),
                resume_from: Some(bytes::Bytes::from(serde_json::to_vec(&token).unwrap())),
                resume_sequence: None,
                request_position_handle: false,
            })
            .await;
        let error = result
            .err()
            .expect("incomplete row cursors must not fall back");
        assert!(format!("{error:#}").contains("Incomplete MySQL row position"));
        assert!(source.subscriber_resume_positions.read().await.is_empty());
    }

    mod position_comparator_tests {
        use bytes::Bytes;
        use drasi_lib::sources::PositionComparator;

        use crate::types::{MySqlPositionComparator, ReplicationState};

        fn make_position(file: &str, pos: u32, gtid: Option<&str>, ts: u64) -> Bytes {
            let state = ReplicationState::new(file, pos, gtid.map(str::to_string), ts);
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
        fn test_bootstrap_boundary_uses_native_cursor_not_timestamp() {
            let comparator = MySqlPositionComparator;
            let boundary = make_position("mysql-bin.000001", 200, None, 0);
            let event = make_position("mysql-bin.000001", 200, None, 2000);
            assert!(
                !comparator.position_reached(&event, &boundary),
                "a completed snapshot cursor must suppress the same committed transaction"
            );
        }

        #[test]
        fn test_row_offsets_and_completed_boundary_are_strictly_ordered() {
            let comparator = MySqlPositionComparator;
            let position = ReplicationState::new("mysql-bin.000001", 200, None, 100);
            let first = position
                .clone()
                .with_transaction_row(100, 0)
                .unwrap()
                .to_position_bytes();
            let second = position
                .with_transaction_row(100, 1)
                .unwrap()
                .to_position_bytes();
            let completed = make_position("mysql-bin.000001", 200, None, 0);

            assert!(comparator.position_reached(&second, &first));
            assert!(!comparator.position_reached(&first, &second));
            assert!(!comparator.position_reached(&second, &second));
            assert!(!comparator.position_reached(&first, &completed));
            assert!(!comparator.position_reached(&second, &completed));
            assert!(comparator.position_reached(&completed, &second));
            assert!(comparator
                .position_reached(&make_position("mysql-bin.000002", 4, None, 0), &completed));
        }

        #[test]
        fn test_current_boundary_token_has_no_row_fields() {
            let bytes = make_position("mysql-bin.000001", 200, Some("uuid:1-7"), 0);
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(json.get("row_offset").is_none());
            assert!(json.get("transaction_start_position").is_none());
            let state = crate::types::decode_position(&bytes).unwrap();
            assert_eq!(state.row_offset, None);
            assert_eq!(state.transaction_start_position, None);
        }

        #[test]
        fn test_malformed_row_positions_are_rejected() {
            let valid = serde_json::json!({
                "binlog_file": "mysql-bin.000001",
                "binlog_position": 200,
                "gtid_set": null,
                "last_processed_timestamp": 100,
                "transaction_start_position": 100,
                "row_offset": 1
            });
            for field in ["row_offset", "transaction_start_position"] {
                let mut value = valid.clone();
                value.as_object_mut().unwrap().remove(field);
                assert!(
                    crate::types::decode_position(&serde_json::to_vec(&value).unwrap()).is_err()
                );
            }
            for start in [0, 3, 200, 201] {
                let mut value = valid.clone();
                value["transaction_start_position"] = serde_json::json!(start);
                assert!(
                    crate::types::decode_position(&serde_json::to_vec(&value).unwrap()).is_err()
                );
            }
            let mut value = valid;
            value["binlog_file"] = serde_json::json!("");
            assert!(crate::types::decode_position(&serde_json::to_vec(&value).unwrap()).is_err());
        }

        #[test]
        fn test_native_position_precedes_higher_timestamp() {
            let comparator = MySqlPositionComparator;
            let resume = make_position("mysql-bin.000001", 100, None, 1000);
            let event = make_position("mysql-bin.000001", 50, None, 2000);
            assert!(!comparator.position_reached(&event, &resume));
        }

        #[test]
        fn test_newer_native_position_passes_despite_lower_timestamp() {
            let comparator = MySqlPositionComparator;
            let resume = make_position("mysql-bin.000001", 100, None, 2000);
            let event = make_position("mysql-bin.000001", 200, None, 1000);
            assert!(comparator.position_reached(&event, &resume));
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
            let state = ReplicationState::new(
                "mysql-bin.000003",
                456,
                Some("abc-123:1-10".to_string()),
                1700000000,
            );
            let bytes = state.to_position_bytes();
            let recovered = ReplicationState::from_position_bytes(&bytes).unwrap();
            assert_eq!(recovered.binlog_file, "mysql-bin.000003");
            assert_eq!(recovered.binlog_position, 456);
            assert_eq!(recovered.gtid_set, Some("abc-123:1-10".to_string()));
            assert_eq!(recovered.last_processed_timestamp, 1700000000);
        }
    }
}
