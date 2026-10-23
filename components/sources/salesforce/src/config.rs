use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

use drasi_salesforce_common::{InitialCursorBehavior, SalesforceConnectionConfig};

pub(crate) const MAX_OBJECTS: usize = 32;

fn default_pubsub_endpoint() -> String {
    "https://api.pubsub.salesforce.com:7443".to_string()
}

fn default_reconcile_interval_seconds() -> u64 {
    5
}

fn default_full_scan_interval_seconds() -> u64 {
    60
}

fn default_max_rows() -> usize {
    100_000
}

fn default_max_events() -> usize {
    1_000_000
}

fn default_retention_hours() -> u64 {
    96
}

fn default_max_projected_row_bytes() -> usize {
    1_048_576
}

fn default_max_scan_bytes() -> usize {
    8 * 1024 * 1024
}

fn default_max_replay_page_events() -> usize {
    100
}

fn default_max_replay_page_bytes() -> usize {
    1_048_576
}

fn default_max_subscribers() -> usize {
    16
}

fn default_snapshot_ticket_ttl_seconds() -> u64 {
    600
}

fn default_max_snapshot_leases() -> usize {
    16
}

fn default_max_spool_bytes() -> u64 {
    8 * 1024 * 1024 * 1024
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SalesforceSourceConfig {
    pub connection: SalesforceConnectionConfig,
    pub objects: Vec<String>,
    #[serde(default = "default_pubsub_endpoint")]
    pub pubsub_endpoint: String,
    #[serde(default = "default_reconcile_interval_seconds")]
    pub reconcile_interval_seconds: u64,
    #[serde(default = "default_full_scan_interval_seconds")]
    pub full_scan_interval_seconds: u64,
    #[serde(default = "default_max_rows")]
    pub max_rows: usize,
    #[serde(default = "default_max_events")]
    pub max_events: usize,
    #[serde(default = "default_retention_hours")]
    pub retention_hours: u64,
    #[serde(default = "default_max_projected_row_bytes")]
    pub max_projected_row_bytes: usize,
    #[serde(default = "default_max_scan_bytes")]
    pub max_scan_bytes: usize,
    #[serde(default = "default_max_replay_page_events")]
    pub max_replay_page_events: usize,
    #[serde(default = "default_max_replay_page_bytes")]
    pub max_replay_page_bytes: usize,
    #[serde(default = "default_max_subscribers")]
    pub max_subscribers: usize,
    pub spool_root: String,
    #[serde(default = "default_snapshot_ticket_ttl_seconds")]
    pub snapshot_ticket_ttl_seconds: u64,
    #[serde(default = "default_max_snapshot_leases")]
    pub max_snapshot_leases: usize,
    #[serde(default = "default_max_spool_bytes")]
    pub max_spool_bytes: u64,
    #[serde(default)]
    pub initial_cursor_behavior: InitialCursorBehavior,
}

impl SalesforceSourceConfig {
    pub fn validate(&self) -> Result<()> {
        self.connection.validate()?;
        ensure!(
            !self.objects.is_empty() && self.objects.len() <= MAX_OBJECTS,
            "objects must contain between 1 and {MAX_OBJECTS} Salesforce object names"
        );
        let mut unique_objects = HashSet::new();
        for object in &self.objects {
            drasi_salesforce_common::projection::validate_identifier(object)?;
            ensure!(
                unique_objects.insert(object),
                "duplicate Salesforce object '{object}'"
            );
        }

        ensure!(
            !self.spool_root.trim().is_empty(),
            "spool_root must not be empty"
        );
        ensure!(
            self.reconcile_interval_seconds > 0,
            "reconcile_interval_seconds must be greater than zero"
        );
        ensure!(
            self.full_scan_interval_seconds > 0,
            "full_scan_interval_seconds must be greater than zero"
        );
        ensure!(self.max_rows > 0, "max_rows must be greater than zero");
        ensure!(
            self.max_rows <= tokio::sync::Semaphore::MAX_PERMITS,
            "max_rows exceeds the supported invalidation budget"
        );
        ensure!(self.max_events > 0, "max_events must be greater than zero");
        ensure!(
            self.retention_hours > 0,
            "retention_hours must be greater than zero"
        );
        ensure!(
            self.max_projected_row_bytes > 0,
            "max_projected_row_bytes must be greater than zero"
        );
        ensure!(
            self.max_scan_bytes > 0,
            "max_scan_bytes must be greater than zero"
        );
        ensure!(
            self.max_replay_page_events > 0,
            "max_replay_page_events must be greater than zero"
        );
        ensure!(
            self.max_replay_page_bytes > 0,
            "max_replay_page_bytes must be greater than zero"
        );
        ensure!(
            self.max_subscribers > 0,
            "max_subscribers must be greater than zero"
        );
        ensure!(
            self.snapshot_ticket_ttl_seconds > 0,
            "snapshot_ticket_ttl_seconds must be greater than zero"
        );
        ensure!(
            self.max_snapshot_leases > 0,
            "max_snapshot_leases must be greater than zero"
        );
        ensure!(
            self.max_spool_bytes > 0,
            "max_spool_bytes must be greater than zero"
        );
        Ok(())
    }

    pub fn source_identity(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"salesforce-graph-v2|");
        digest.update(self.connection.expected_org_id.as_bytes());
        digest.update(b"|");
        digest.update(self.connection.api_version.as_bytes());
        digest.update(b"|");

        let mut objects = self.objects.clone();
        objects.sort();
        for object in objects {
            digest.update(object.as_bytes());
            digest.update(b",");
        }

        format!("{:x}", digest.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> SalesforceSourceConfig {
        serde_json::from_value(json!({
            "connection": {
                "instanceUrl": "https://example.my.salesforce.com",
                "expectedOrgId": "00D000000000001AAA",
                "auth": {
                    "type": "clientCredentials",
                    "tokenUrl": "https://example.my.salesforce.com/services/oauth2/token",
                    "clientId": "fixture",
                    "clientSecret": "fixture"
                }
            },
            "objects": ["Account", "Contact"],
            "spoolRoot": "fixture"
        }))
        .unwrap()
    }

    #[test]
    fn object_selection_is_validated_and_bounded() {
        let mut config = config();
        config.validate().unwrap();
        for invalid in [
            vec![],
            vec!["Account".into(), "Account".into()],
            vec!["Account WHERE Name = 'x'".into()],
            (0..=MAX_OBJECTS)
                .map(|index| format!("Object{index}"))
                .collect(),
        ] {
            config.objects = invalid;
            assert!(config.validate().is_err());
        }
        config.objects = vec!["Account".into()];
        config.max_rows = usize::MAX;
        assert!(config.validate().is_err());
    }

    #[test]
    fn source_identity_binds_selection_not_order_or_resource_limits() {
        let mut config = config();
        let original = config.source_identity();
        config.objects.reverse();
        config.max_rows *= 2;
        assert_eq!(original, config.source_identity());
        config.objects.pop();
        assert_ne!(original, config.source_identity());
    }

    #[test]
    fn metadata_configuration_round_trips_without_a_projection() {
        let config = config();
        let value = serde_json::to_value(&config).unwrap();
        assert!(value.get("projection").is_none());
        assert_eq!(
            serde_json::from_value::<SalesforceSourceConfig>(value).unwrap(),
            config
        );
    }
}
