#[cfg(test)]
mod tests {
    use super::super::*;

    #[test]
    fn test_bootstrap_builder_with_valid_config() {
        let provider = MySqlBootstrapProvider::builder()
            .with_host("localhost")
            .with_database("testdb")
            .with_user("testuser")
            .with_password("testpass")
            .with_tables(vec!["users".to_string()])
            .build();
        assert!(provider.is_ok());
    }

    #[test]
    fn test_bootstrap_config_validation_missing_database() {
        let config = MySqlBootstrapConfig {
            host: "localhost".to_string(),
            port: 3306,
            database: String::new(),
            user: "user".to_string(),
            password: String::new(),
            tables: vec!["users".to_string()],
            table_keys: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_bootstrap_config_validation_missing_tables() {
        let config = MySqlBootstrapConfig {
            host: "localhost".to_string(),
            port: 3306,
            database: "test".to_string(),
            user: "user".to_string(),
            password: String::new(),
            tables: vec![],
            table_keys: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_bootstrap_config_validation_invalid_table_name() {
        let config = MySqlBootstrapConfig {
            host: "localhost".to_string(),
            port: 3306,
            database: "test".to_string(),
            user: "user".to_string(),
            password: String::new(),
            tables: vec!["users;drop".to_string()],
            table_keys: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_quote_identifier_escapes_backticks() {
        assert_eq!(crate::mysql::quote_identifier("evil`name"), "`evil``name`");
    }
}
