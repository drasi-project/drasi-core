use serde::{Deserialize, Serialize};
use anyhow::Result;
use mongodb::bson::Document;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MongoSourceConfig {
    pub connection_string: String,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub collection: Option<String>,
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
        let mut cols = self.collections.clone();
        if let Some(c) = &self.collection {
            if !cols.contains(c) {
                cols.push(c.clone());
            }
        }
        cols
    }
    pub fn validate(&self) -> Result<()> {
        if self.connection_string.is_empty() {
             return Err(anyhow::anyhow!("Validation error: connection_string cannot be empty"));
        }
        
        let mut connection_db = None;
        if let Ok(url) = url::Url::parse(&self.connection_string) {
            let path = url.path().trim_start_matches('/');
            if !path.is_empty() {
                connection_db = Some(path.to_string());
            }
        }

        match (&self.database, &connection_db) {
            (Some(config_db), Some(conn_db)) => {
                if config_db != conn_db {
                    return Err(anyhow::anyhow!(
                        "Ambiguous configuration: database specified in both config ('{}') and connection string ('{}') but they differ",
                        config_db, conn_db
                    ));
                }
            }
            (None, None) => {
                 return Err(anyhow::anyhow!("Validation error: database must be specified in either config or connection string"));
            }
            _ => {}
        }

        if self.get_collections().is_empty() {
            return Err(anyhow::anyhow!("Validation error: at least one collection must be specified"));
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
            database: Some("db".to_string()),
            collection: Some("col".to_string()),
            collections: vec![],
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
            database: Some("db2".to_string()),
            collection: Some("col".to_string()),
            collections: vec![],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validation_only_config() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost".to_string(),
            database: Some("db".to_string()),
            collection: Some("col".to_string()),
            collections: vec![],
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
            database: None,
            collection: Some("col".to_string()),
            collections: vec![],
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
            database: None,
            collection: Some("col".to_string()),
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
            connection_string: "mongodb://localhost/db".to_string(),
            database: None,
            collection: None,
            collections: vec!["col1".to_string(), "col2".to_string()],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_ok());
        assert_eq!(config.get_collections().len(), 2);
    }
    
    #[test]
    fn test_config_validation_mixed_collection() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost/db".to_string(),
            database: None,
            collection: Some("col1".to_string()),
            collections: vec!["col2".to_string()],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_ok());
        let cols = config.get_collections();
        assert_eq!(cols.len(), 2);
        assert!(cols.contains(&"col1".to_string()));
        assert!(cols.contains(&"col2".to_string()));
    }
    
    #[test]
    fn test_config_validation_no_collection() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost/db".to_string(),
            database: None,
            collection: None,
            collections: vec![],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_err());
    }
}
