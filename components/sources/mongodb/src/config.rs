use serde::{Deserialize, Serialize};
use anyhow::Result;
use mongodb::bson::Document;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MongoSourceConfig {
    pub connection_string: String,
    pub database: String,
    pub collection: String,
    #[serde(default)]
    pub pipeline: Option<Vec<Document>>,
}

impl MongoSourceConfig {
    pub fn validate(&self) -> Result<()> {
        if self.connection_string.is_empty() {
             return Err(anyhow::anyhow!("Validation error: connection_string cannot be empty"));
        }
        if self.database.is_empty() {
            return Err(anyhow::anyhow!("Validation error: database cannot be empty"));
        }
        if self.collection.is_empty() {
            return Err(anyhow::anyhow!("Validation error: collection cannot be empty"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_validation() {
        let config = MongoSourceConfig {
            connection_string: "mongodb://localhost".to_string(),
            database: "db".to_string(),
            collection: "col".to_string(),
            pipeline: None,
        };
        assert!(config.validate().is_ok());

        let invalid_config = MongoSourceConfig {
            connection_string: "".to_string(),
            database: "db".to_string(),
            collection: "col".to_string(),
            pipeline: None,
        };
        assert!(invalid_config.validate().is_err());
    }
}
