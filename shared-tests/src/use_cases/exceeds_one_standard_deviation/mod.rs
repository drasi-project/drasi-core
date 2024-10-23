#![allow(clippy::unwrap_used)]
// Copyright 2024 The Drasi Authors.
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

use std::sync::Arc;

use serde_json::json;

use drasi_core::{
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};

use self::data::get_bootstrap_data;
use crate::QueryTestConfig;

mod data;
mod queries;

async fn bootstrap_query(query: &ContinuousQuery) {
    let data = get_bootstrap_data();

    for change in data {
        let _ = query.process_source_change(change).await;
    }
}

pub async fn exceeds_one_standard_deviation(config: &(impl QueryTestConfig + Send)) {
    let exceeds_one_standard_deviation_query = {
        let mut builder = QueryBuilder::new(queries::exceeds_one_standard_deviation_query())
            .with_joins(queries::exceeds_one_standard_deviation_metadata());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    bootstrap_query(&exceeds_one_standard_deviation_query).await;

    let mut timestamp = 1696150800;

    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": 45.0  }),
                ),
            },
        };

        let result = exceeds_one_standard_deviation_query
            .process_source_change(change)
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
        timestamp += 24 * 60 * 60;
    }

    {
        // #1
        // Average: 43
        // Standard Deviation: 2
        // No result
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": (41.0)}),
                ),
            },
        };

        let result = exceeds_one_standard_deviation_query
            .process_source_change(change)
            .await
            .unwrap();

        assert_eq!(result.len(), 0);

        timestamp += 24 * 60 * 60;
    }

    {
        // #2
        // Average: 43.33333
        // Standard Deviation: 1.699673171197595
        // No result
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": (44.0)}),
                ),
            },
        };

        let result = exceeds_one_standard_deviation_query
            .process_source_change(change)
            .await
            .unwrap();

        assert_eq!(result.len(), 0);

        timestamp += 24 * 60 * 60;
    }
    {
        // #3
        // Average 43.3125
        // Standard Deviation: 1.4724023736737184
        // No result
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": (43.25)}),
                ),
            },
        };

        let result = exceeds_one_standard_deviation_query
            .process_source_change(change)
            .await
            .unwrap();

        assert_eq!(result.len(), 0);

        timestamp += 24 * 60 * 60;
    }

    {
        // Average: 42.65
        // Standard Deviation: 1.8681541692269403
        // Should be getting one result
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": 40.0  }),
                ),
            },
        };

        let result = exceeds_one_standard_deviation_query
            .process_source_change(change)
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
    }
}
