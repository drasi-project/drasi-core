use serde::{Deserialize, Serialize};
use anyhow::Result;
use mongodb::bson::Document;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MongoSourceConfig {
    pub connection_string: String,
    pub database: String,
    #[serde(default)]
    pub collections: Vec<String>,
    #[serde(default)]
    pub pipeline: Option<Vec<Document>>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

impl MongoSourceConfig {
    pub fn get_collections(&self) -> Vec<String> {
        self.collections.clone()
    }

    pub fn validate(&self) -> Result<()> {
        if self.connection_string.is_empty() {
            return Err(anyhow::anyhow!("Validation error: connection_string cannot be empty"));
        }

        if self.database.is_empty() {
            return Err(anyhow::anyhow!("Validation error: database cannot be empty"));
        }

        if self.collections.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: at least one collection must be specified in 'collections' field"
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_validation_match() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost/db".to_string(),
            database: "db".to_string(),
            collections: vec!["col".to_string()],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_validation_mismatch() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost/db1".to_string(),
            database: "db".to_string(),
            collections: vec!["col".to_string()],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_validation_only_config() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost".to_string(),
            database: "db".to_string(),
            collections: vec!["col".to_string()],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_validation_only_connection() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost/db".to_string(),
            database: "db".to_string(),
            collections: vec!["col".to_string()],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_validation_none() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost".to_string(),
            database: String::new(),
            collections: vec![],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validation_multi_collection() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost".to_string(),
            database: "db".to_string(),
            collections: vec!["col1".to_string(), "col2".to_string()],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_ok());
        assert_eq!(config.get_collections().len(), 2);
    }

    #[test]
    fn test_config_validation_no_collection() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost/db".to_string(),
            database: "db".to_string(),
            collections: vec![],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_collections_only_single() {
        let json = r#"{
            "connection_string": "mongodb://localhost/db",
            "database": "mydb",
            "collections": ["my_collection"]
        }"#;
        let config: MongoSourceConfig = serde_json::from_str(json).unwrap();

        assert!(config.validate().is_ok());
        assert_eq!(config.get_collections(), vec!["my_collection"]);
        assert_eq!(config.collections, vec!["my_collection"]);
    }

    #[test]
    fn test_collections_only_multiple() {
        let json = r#"{
            "connection_string": "mongodb://localhost/db",
            "database": "mydb",
            "collections": ["col1", "col2", "col3"]
        }"#;
        let config: MongoSourceConfig = serde_json::from_str(json).unwrap();

        assert!(config.validate().is_ok());
        let cols = config.get_collections();
        assert_eq!(cols.len(), 3);
        assert!(cols.contains(&"col1".to_string()));
        assert!(cols.contains(&"col2".to_string()));
        assert!(cols.contains(&"col3".to_string()));
    }

    #[test]
    fn test_neither_field_error() {
        let json = r#"{
            "connection_string": "mongodb://localhost/db",
            "database": "mydb"
        }"#;
        let config: MongoSourceConfig = serde_json::from_str(json).unwrap();

        assert!(config.validate().is_err());
        assert_eq!(config.collections.len(), 0);
    }
}
