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
