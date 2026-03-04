// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! OData response types for Dataverse change tracking.
//!
//! These types represent the OData Web API response format, which is the
//! REST equivalent of the platform's `RetrieveEntityChangesResponse`.
//!
//! # Mapping to Platform SDK Types
//!
//! | Platform SDK                  | OData Web API                   |
//! |-------------------------------|---------------------------------|
//! | `NewOrUpdatedItem`            | Record in `value[]` without     |
//! |                               | `$deletedEntity` in context     |
//! | `RemovedOrDeletedItem`        | Record with `@odata.context`    |
//! |                               | containing `$deletedEntity`     |
//! | `DataToken` (string)          | `@odata.deltaLink` URL with     |
//! |                               | embedded delta token            |
//! | `MoreRecords` + `PagingCookie`| `@odata.nextLink` URL           |

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// OData delta response from the Dataverse Web API.
///
/// Equivalent to `RetrieveEntityChangesResponse` from the platform SDK.
/// Contains changed records (new/updated and deleted) plus a delta link
/// for the next poll.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ODataDeltaResponse {
    /// OData context URL (contains entity set name and metadata path).
    #[serde(rename = "@odata.context", default)]
    pub context: Option<String>,

    /// List of changed records. Each entry is either:
    /// - A new/updated record (regular entity object)
    /// - A deleted record (object with `@odata.context` containing `$deletedEntity`)
    #[serde(default)]
    pub value: Vec<Value>,

    /// Delta link URL for the next change tracking poll.
    /// Contains an embedded delta token equivalent to the platform's `DataToken`.
    #[serde(rename = "@odata.deltaLink", default)]
    pub delta_link: Option<String>,

    /// Next link URL for pagination within the current response.
    /// Equivalent to the platform's `MoreRecords` + `PagingCookie`.
    #[serde(rename = "@odata.nextLink", default)]
    pub next_link: Option<String>,
}

/// Classified change from a Dataverse delta response.
///
/// Maps to the platform's `IChangedItem` interface with
/// `ChangeType.NewOrUpdated` and `ChangeType.RemoveOrDeleted` variants.
#[derive(Debug, Clone)]
pub enum DataverseChange {
    /// A new or updated entity record.
    /// Equivalent to `NewOrUpdatedItem` in the platform SDK.
    NewOrUpdated {
        /// Entity ID (GUID string).
        id: String,
        /// Entity logical name (used as the node label).
        entity_name: String,
        /// All attribute key-value pairs.
        attributes: serde_json::Map<String, Value>,
    },
    /// A deleted entity record.
    /// Equivalent to `RemovedOrDeletedItem` in the platform SDK.
    Deleted {
        /// Entity ID (GUID string).
        id: String,
        /// Entity logical name (used as the node label).
        entity_name: String,
    },
}

/// Extract the entity ID from an OData record.
///
/// Dataverse entity IDs follow the pattern `{entitylogicalname}id`.
/// For example, `account` → `accountid`, `contact` → `contactid`.
///
/// # Arguments
///
/// * `record` - The JSON object representing the entity
/// * `entity_name` - The entity logical name
pub fn extract_entity_id(
    record: &serde_json::Map<String, Value>,
    entity_name: &str,
) -> Option<String> {
    let id_field = format!("{entity_name}id");
    record
        .get(&id_field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Check if a delta response record represents a deleted entity.
///
/// In OData change tracking, deleted entities are identified by having an
/// `@odata.context` field containing `$deletedEntity`.
///
/// This matches the platform's `ChangeType.RemoveOrDeleted` detection.
pub fn is_deleted_entity(record: &Value) -> bool {
    record
        .get("@odata.context")
        .and_then(|v| v.as_str())
        .is_some_and(|ctx| ctx.contains("$deletedEntity"))
}

/// Parse a delta response into classified changes.
///
/// Processes each record in the delta response `value` array and classifies
/// it as either `NewOrUpdated` or `Deleted`, mirroring the platform's
/// `EntityChanges.Changes` collection.
///
/// # Arguments
///
/// * `response` - The OData delta response
/// * `entity_name` - The entity logical name (used for ID extraction and labeling)
pub fn parse_delta_changes(
    response: &ODataDeltaResponse,
    entity_name: &str,
) -> Vec<DataverseChange> {
    let mut changes = Vec::new();

    for record in &response.value {
        if is_deleted_entity(record) {
            // Deleted entity: extract ID from the "id" field
            if let Some(id) = record.get("id").and_then(|v| v.as_str()) {
                changes.push(DataverseChange::Deleted {
                    id: id.to_string(),
                    entity_name: entity_name.to_string(),
                });
            }
        } else if let Some(obj) = record.as_object() {
            // New or updated entity: extract ID and attributes
            if let Some(id) = extract_entity_id(obj, entity_name) {
                // Filter out OData metadata fields from attributes
                let attributes: serde_json::Map<String, Value> = obj
                    .iter()
                    .filter(|(key, _)| !key.starts_with("@odata."))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();

                changes.push(DataverseChange::NewOrUpdated {
                    id,
                    entity_name: entity_name.to_string(),
                    attributes,
                });
            }
        }
    }

    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_delta_response() {
        let json = r#"{
            "@odata.context": "https://myorg.crm.dynamics.com/api/data/v9.2/$metadata#accounts",
            "@odata.deltaLink": "https://myorg.crm.dynamics.com/api/data/v9.2/accounts?$deltatoken=12345",
            "value": [
                {
                    "@odata.etag": "W/\"12345\"",
                    "accountid": "60c4e274-0d87-e711-80e5-00155db19e6d",
                    "name": "Contoso Ltd",
                    "revenue": 1000000.0
                }
            ]
        }"#;

        let response: ODataDeltaResponse = serde_json::from_str(json).expect("should parse");
        assert_eq!(response.value.len(), 1);
        assert!(response.delta_link.is_some());
        assert!(response.next_link.is_none());

        let changes = parse_delta_changes(&response, "account");
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            DataverseChange::NewOrUpdated {
                id,
                entity_name,
                attributes,
            } => {
                assert_eq!(id, "60c4e274-0d87-e711-80e5-00155db19e6d");
                assert_eq!(entity_name, "account");
                assert_eq!(
                    attributes.get("name").and_then(|v| v.as_str()),
                    Some("Contoso Ltd")
                );
                // OData metadata should be filtered out
                assert!(!attributes.contains_key("@odata.etag"));
            }
            _ => panic!("Expected NewOrUpdated"),
        }
    }

    #[test]
    fn test_parse_delta_with_deletes() {
        let json = r#"{
            "@odata.context": "https://myorg.crm.dynamics.com/api/data/v9.2/$metadata#accounts",
            "@odata.deltaLink": "https://myorg.crm.dynamics.com/api/data/v9.2/accounts?$deltatoken=12346",
            "value": [
                {
                    "@odata.etag": "W/\"99999\"",
                    "accountid": "aaaa-bbbb",
                    "name": "Updated Corp",
                    "revenue": 2000000.0
                },
                {
                    "@odata.context": "https://myorg.crm.dynamics.com/api/data/v9.2/$metadata#accounts/$deletedEntity",
                    "id": "cccc-dddd",
                    "reason": "deleted"
                }
            ]
        }"#;

        let response: ODataDeltaResponse = serde_json::from_str(json).expect("should parse");
        let changes = parse_delta_changes(&response, "account");
        assert_eq!(changes.len(), 2);

        match &changes[0] {
            DataverseChange::NewOrUpdated { id, .. } => assert_eq!(id, "aaaa-bbbb"),
            _ => panic!("Expected NewOrUpdated"),
        }
        match &changes[1] {
            DataverseChange::Deleted { id, entity_name } => {
                assert_eq!(id, "cccc-dddd");
                assert_eq!(entity_name, "account");
            }
            _ => panic!("Expected Deleted"),
        }
    }

    #[test]
    fn test_is_deleted_entity() {
        let deleted = serde_json::json!({
            "@odata.context": "https://test.crm.dynamics.com/api/data/v9.2/$metadata#accounts/$deletedEntity",
            "id": "123",
            "reason": "deleted"
        });
        assert!(is_deleted_entity(&deleted));

        let normal = serde_json::json!({
            "@odata.etag": "W/\"123\"",
            "accountid": "456",
            "name": "Test"
        });
        assert!(!is_deleted_entity(&normal));
    }

    #[test]
    fn test_extract_entity_id() {
        let record = serde_json::json!({
            "accountid": "60c4e274-0d87-e711-80e5-00155db19e6d",
            "name": "Test"
        });
        let obj = record.as_object().expect("should be object");
        assert_eq!(
            extract_entity_id(obj, "account"),
            Some("60c4e274-0d87-e711-80e5-00155db19e6d".to_string())
        );
    }

    #[test]
    fn test_extract_entity_id_missing() {
        let record = serde_json::json!({"name": "Test"});
        let obj = record.as_object().expect("should be object");
        assert_eq!(extract_entity_id(obj, "account"), None);
    }

    #[test]
    fn test_empty_delta_response() {
        let json = r#"{
            "@odata.context": "https://test.crm.dynamics.com/api/data/v9.2/$metadata#accounts",
            "@odata.deltaLink": "https://test.crm.dynamics.com/api/data/v9.2/accounts?$deltatoken=999",
            "value": []
        }"#;

        let response: ODataDeltaResponse = serde_json::from_str(json).expect("should parse");
        let changes = parse_delta_changes(&response, "account");
        assert!(changes.is_empty());
    }

    #[test]
    fn test_pagination_response() {
        let json = r#"{
            "@odata.context": "https://test.crm.dynamics.com/api/data/v9.2/$metadata#accounts",
            "@odata.nextLink": "https://test.crm.dynamics.com/api/data/v9.2/accounts?$skiptoken=abc",
            "value": [
                {"accountid": "111", "name": "Page1Record"}
            ]
        }"#;

        let response: ODataDeltaResponse = serde_json::from_str(json).expect("should parse");
        assert!(response.delta_link.is_none());
        assert!(response.next_link.is_some());
    }
}
