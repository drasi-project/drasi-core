use serde::{Deserialize, Serialize};
use anyhow::Result;
use mongodb::bson::Document;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MongoSourceConfig {
    pub connection_string: String,
    #[serde(default)]
    pub database: Option<String>,
    
    /// DEPRECATED: Use `collections` instead. This field will be removed in a future version.
    /// For backward compatibility, this field is automatically migrated to `collections` during deserialization.
    #[serde(skip_serializing, default)]
    #[deprecated(since = "0.2.0", note = "use `collections` instead")]
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

impl<'de> Deserialize<'de> for MongoSourceConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct ConfigHelper {
            connection_string: String,
            #[serde(default)]
            database: Option<String>,
            #[serde(default)]
            collection: Option<String>,
            #[serde(default)]
            collections: Vec<String>,
            #[serde(default)]
            pipeline: Option<Vec<Document>>,
            #[serde(default)]
            username: Option<String>,
            #[serde(default)]
            password: Option<String>,
        }
        
        let helper = ConfigHelper::deserialize(deserializer)?;
        
        // Migrate collection -> collections
        let mut collections = helper.collections;
        if let Some(ref col) = helper.collection {
            if !collections.contains(col) {
                collections.push(col.clone());
            }
            // Log deprecation warning
            log::warn!(
                "Config field 'collection' is deprecated and will be removed in a future version. \
                 Use 'collections' instead. Automatically migrating '{}' to collections list.",
                col
            );
        }
        
        
        #[allow(deprecated)]
        let config = MongoSourceConfig {
            connection_string: helper.connection_string,
            database: helper.database,
            collection: helper.collection,
            collections,
            pipeline: helper.pipeline,
            username: helper.username,
            password: helper.password,
        };
        Ok(config)
    }
}

impl MongoSourceConfig {
    pub fn get_collections(&self) -> Vec<String> {
        // Simply return collections since migration happens in deserializer
        self.collections.clone()
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
        
        // Warn if deprecated field is still set in memory (not from deserialization)
        #[allow(deprecated)]
        if self.collection.is_some() && !self.collections.is_empty() {
            #[allow(deprecated)]
            let col = self.collection.as_ref().unwrap();
            if !self.collections.contains(col) {
                log::warn!(
                    "Both 'collection' and 'collections' are set. The 'collection' field is deprecated. \
                     Merging '{}' into collections list.",
                    col
                );
            }
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
    #[allow(deprecated)]
    fn test_config_validation_match() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost/db".to_string(),
            database: Some("db".to_string()),
            collection: None,
            collections: vec!["col".to_string()],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    #[allow(deprecated)]
    fn test_config_validation_mismatch() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost/db1".to_string(),
            database: Some("db2".to_string()),
            collection: None,
            collections: vec!["col".to_string()],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    #[allow(deprecated)]
    fn test_config_validation_only_config() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost".to_string(),
            database: Some("db".to_string()),
            collection: None,
            collections: vec!["col".to_string()],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    #[allow(deprecated)]
    fn test_config_validation_only_connection() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost/db".to_string(),
            database: None,
            collection: None,
            collections: vec!["col".to_string()],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    #[allow(deprecated)]
    fn test_config_validation_none() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost".to_string(),
            database: None,
            collection: None,
            collections: vec![],
            pipeline: None,
            username: None,
            password: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    #[allow(deprecated)]
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
    #[allow(deprecated)]
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

    // New tests for backward compatibility
    #[test]
    fn test_collections_only_single() {
        let json = r#"{
            "connection_string": "mongodb://localhost/db",
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
    fn test_backward_compat_collection_migrated() {
        // Simulate deserialization from JSON with old 'collection' field
        let json = r#"{
            "connection_string": "mongodb://localhost/db",
            "collection": "old_col"
        }"#;
        let config: MongoSourceConfig = serde_json::from_str(json).unwrap();
        
        assert!(config.validate().is_ok());
        assert_eq!(config.get_collections(), vec!["old_col"]);
        assert_eq!(config.collections, vec!["old_col"]); // Auto-migrated
    }

    #[test]
    fn test_both_fields_merged_no_duplicate() {
        let json = r#"{
            "connection_string": "mongodb://localhost/db",
            "collection": "col1",
            "collections": ["col1", "col2"]
        }"#;
        let config: MongoSourceConfig = serde_json::from_str(json).unwrap();
        
        let cols = config.get_collections();
        assert_eq!(cols.len(), 2); // No duplicate
        assert!(cols.contains(&"col1".to_string()));
        assert!(cols.contains(&"col2".to_string()));
    }

    #[test]
    fn test_both_fields_merged_different() {
        let json = r#"{
            "connection_string": "mongodb://localhost/db",
            "collection": "col1",
            "collections": ["col2", "col3"]
        }"#;
        let config: MongoSourceConfig = serde_json::from_str(json).unwrap();
        
        let cols = config.get_collections();
        assert_eq!(cols.len(), 3);
        assert!(cols.contains(&"col1".to_string()));  // Migrated from collection
        assert!(cols.contains(&"col2".to_string()));
        assert!(cols.contains(&"col3".to_string()));
    }

    #[test]
    fn test_neither_field_error() {
        let json = r#"{
            "connection_string": "mongodb://localhost/db"
        }"#;
        let config: MongoSourceConfig = serde_json::from_str(json).unwrap();
        
        assert!(config.validate().is_err());
        assert_eq!(config.collections.len(), 0);
    }
