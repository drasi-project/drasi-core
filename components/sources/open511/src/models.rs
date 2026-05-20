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

//! Open511 response model types.

use serde::{Deserialize, Serialize};

/// `/events` collection response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Open511EventsResponse {
    #[serde(default)]
    pub events: Vec<Open511Event>,
    #[serde(default)]
    pub pagination: Option<Open511Pagination>,
    #[serde(default)]
    pub meta: Option<Open511Meta>,
}

/// Pagination metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Open511Pagination {
    #[serde(default)]
    pub offset: Option<serde_json::Value>,
    #[serde(default)]
    pub next_url: Option<String>,
}

/// Response metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Open511Meta {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub up_url: Option<String>,
}

/// Open511 event object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Open511Event {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub jurisdiction_url: Option<String>,
    #[serde(default)]
    pub headline: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub event_type: Option<String>,
    #[serde(default)]
    pub event_subtypes: Option<Vec<String>>,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub updated: Option<String>,
    #[serde(default)]
    pub geography: Option<Open511Geography>,
    #[serde(default)]
    pub roads: Option<Vec<Open511Road>>,
    #[serde(default)]
    pub areas: Option<Vec<Open511Area>>,
    #[serde(default)]
    pub schedule: Option<Open511Schedule>,
    #[serde(rename = "+ivr_message", default)]
    pub ivr_message: Option<String>,
    #[serde(rename = "+linear_reference_km", default)]
    pub linear_reference_km: Option<serde_json::Value>,
}

impl Open511Event {
    /// Returns the most recent known timestamp (`updated` first, then `created`).
    pub fn updated_or_created(&self) -> Option<&str> {
        self.updated.as_deref().or(self.created.as_deref())
    }
}

/// Event geography payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Open511Geography {
    #[serde(rename = "type")]
    pub geometry_type: String,
    pub coordinates: serde_json::Value,
}

/// Affected road object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Open511Road {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(rename = "+delay", default)]
    pub delay: Option<String>,
}

/// Affected area object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Open511Area {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// Event schedule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Open511Schedule {
    #[serde(default)]
    pub intervals: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_event_response() {
        let json = serde_json::json!({
            "events": [
                {
                    "id": "drivebc.ca/1",
                    "status": "ACTIVE"
                }
            ]
        });

        let parsed: Open511EventsResponse =
            serde_json::from_value(json).expect("should deserialize minimal response");
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].id, "drivebc.ca/1");
        assert_eq!(parsed.events[0].status, "ACTIVE");
    }

    #[test]
    fn deserialize_full_event_response() {
        let json = serde_json::json!({
            "events": [
                {
                    "id": "drivebc.ca/DBC-53013",
                    "status": "ACTIVE",
                    "headline": "INCIDENT",
                    "event_type": "INCIDENT",
                    "severity": "MAJOR",
                    "created": "2023-06-05T16:08:02-07:00",
                    "updated": "2025-04-10T09:28:05-07:00",
                    "geography": { "type": "Point", "coordinates": [-122.37, 52.21] },
                    "roads": [{ "name": "Highway 1", "direction": "BOTH", "state": "CLOSED" }],
                    "areas": [{ "id": "drivebc.ca/3", "name": "Rocky Mountain District" }],
                    "schedule": { "intervals": ["2023-06-05T23:08/"] },
                    "+ivr_message": "Incident message",
                    "+linear_reference_km": -1
                }
            ],
            "pagination": { "offset": "0" },
            "meta": { "url": "/events", "version": "v1" }
        });

        let parsed: Open511EventsResponse =
            serde_json::from_value(json).expect("should deserialize full response");
        let event = &parsed.events[0];
        assert_eq!(event.id, "drivebc.ca/DBC-53013");
        assert_eq!(
            event.updated_or_created(),
            Some("2025-04-10T09:28:05-07:00")
        );
        assert_eq!(event.roads.as_ref().expect("road").len(), 1);
        assert_eq!(event.areas.as_ref().expect("area").len(), 1);
        assert!(event.ivr_message.is_some());
    }
}
