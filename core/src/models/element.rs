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

use std::{
    fmt::{Display, Formatter},
    sync::Arc,
};

use crate::evaluation::variable_value::VariableValue;

use super::{ElementPropertyMap, ElementValue};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ElementReference {
    pub source_id: Arc<str>,
    pub element_id: Arc<str>,
}

impl ElementReference {
    pub fn new(source_id: &str, element_id: &str) -> Self {
        ElementReference {
            source_id: Arc::from(source_id),
            element_id: Arc::from(element_id),
        }
    }
}

impl Display for ElementReference {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.source_id, self.element_id)
    }
}

/// Timestamp type used for elements, measured in milliseconds since UNIX epoch.
pub type ElementTimestamp = u64;

/// Maximum plausible `effective_from` value in milliseconds.
///
/// Corresponds to approximately year 2286. Any value above this threshold
/// is almost certainly in the wrong unit (e.g., nanoseconds or microseconds
/// instead of milliseconds).
///
/// A nanosecond-scale timestamp from the current era is ~10^18, which exceeds
/// this threshold by 5 orders of magnitude—making the check extremely reliable
/// with zero false positives for any date before year 2286.
pub const MAX_REASONABLE_MILLIS_TIMESTAMP: u64 = 10_000_000_000_000;

/// Validates that an `ElementTimestamp` value is in the expected millisecond range.
///
/// Returns `Ok(())` if the value is zero (used by bootstrap providers for "unknown")
/// or falls below [`MAX_REASONABLE_MILLIS_TIMESTAMP`].
///
/// Returns `Err` with a descriptive message if the value appears to be in the wrong
/// unit (e.g., nanoseconds instead of milliseconds).
///
/// # Examples
/// ```
/// use drasi_core::models::element::validate_effective_from;
///
/// // Valid millisecond timestamp (Feb 2026)
/// assert!(validate_effective_from(1_771_000_000_000).is_ok());
///
/// // Zero is allowed (bootstrap "unknown" sentinel)
/// assert!(validate_effective_from(0).is_ok());
///
/// // Nanosecond timestamp is rejected
/// assert!(validate_effective_from(1_771_000_000_000_000_000).is_err());
/// ```
pub fn validate_effective_from(value: ElementTimestamp) -> Result<(), String> {
    if value == 0 {
        return Ok(());
    }
    if value > MAX_REASONABLE_MILLIS_TIMESTAMP {
        return Err(format!(
            "effective_from value {} ({:.2e}) appears to be in nanoseconds or microseconds, \
             not milliseconds. Expected a value < {} (~year 2286). \
             Use timestamp_millis() instead of timestamp_nanos_opt().",
            value, value as f64, MAX_REASONABLE_MILLIS_TIMESTAMP
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ElementMetadata {
    pub reference: ElementReference,
    pub labels: Arc<[Arc<str>]>,

    /// The effective time from which this element is valid. Measured in milliseconds since UNIX epoch.
    pub effective_from: ElementTimestamp,
}

impl Display for ElementMetadata {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "({}, [{}], {})",
            self.reference,
            self.labels.join(","),
            self.effective_from
        )
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum Element {
    // Incoming changes get turned into an Element
    Node {
        metadata: ElementMetadata,
        properties: ElementPropertyMap,
    },
    Relation {
        metadata: ElementMetadata,
        in_node: ElementReference,
        out_node: ElementReference,
        properties: ElementPropertyMap,
    },
}

impl Element {
    pub fn get_reference(&self) -> &ElementReference {
        match self {
            Element::Node { metadata, .. } => &metadata.reference,
            Element::Relation { metadata, .. } => &metadata.reference,
        }
    }

    pub fn get_effective_from(&self) -> ElementTimestamp {
        match self {
            Element::Node { metadata, .. } => metadata.effective_from,
            Element::Relation { metadata, .. } => metadata.effective_from,
        }
    }

    pub fn get_metadata(&self) -> &ElementMetadata {
        match self {
            Element::Node { metadata, .. } => metadata,
            Element::Relation { metadata, .. } => metadata,
        }
    }

    pub fn get_property(&self, name: &str) -> &ElementValue {
        let props = match self {
            Element::Node { properties, .. } => properties,
            Element::Relation { properties, .. } => properties,
        };
        &props[name]
    }

    pub fn get_properties(&self) -> &ElementPropertyMap {
        match self {
            Element::Node { properties, .. } => properties,
            Element::Relation { properties, .. } => properties,
        }
    }

    pub fn merge_missing_properties(&mut self, other: &Element) {
        match (self, other) {
            (
                Element::Node {
                    properties,
                    metadata,
                },
                Element::Node {
                    properties: other_properties,
                    metadata: other_metadata,
                },
            ) => {
                assert_eq!(metadata.reference, other_metadata.reference);
                properties.merge(other_properties);
            }
            (
                Element::Relation {
                    in_node: _,
                    out_node: _,
                    properties,
                    metadata,
                },
                Element::Relation {
                    in_node: _other_in_node,
                    out_node: _other_out_node,
                    properties: other_properties,
                    metadata: other_metadata,
                },
            ) => {
                assert_eq!(metadata.reference, other_metadata.reference);
                properties.merge(other_properties);
            }
            _ => panic!("Cannot merge different element types"),
        }
    }

    pub fn to_expression_variable(&self) -> VariableValue {
        VariableValue::Element(Arc::new(self.clone()))
    }

    pub fn update_effective_time(&mut self, timestamp: ElementTimestamp) {
        match self {
            Element::Node { metadata, .. } => metadata.effective_from = timestamp,
            Element::Relation { metadata, .. } => metadata.effective_from = timestamp,
        }
    }
}

impl From<&Element> for serde_json::Value {
    fn from(val: &Element) -> Self {
        match val {
            Element::Node {
                metadata,
                properties,
            } => {
                let mut properties: serde_json::Map<String, serde_json::Value> = properties.into();

                properties.insert(
                    "$metadata".to_string(),
                    serde_json::Value::String(metadata.to_string()),
                );

                serde_json::Value::Object(properties)
            }
            Element::Relation {
                metadata,
                in_node,
                out_node,
                properties,
            } => {
                let mut properties: serde_json::Map<String, serde_json::Value> = properties.into();

                properties.insert(
                    "$metadata".to_string(),
                    serde_json::Value::String(metadata.to_string()),
                );

                properties.insert(
                    "$in_node".to_string(),
                    serde_json::Value::String(in_node.to_string()),
                );
                properties.insert(
                    "$out_node".to_string(),
                    serde_json::Value::String(out_node.to_string()),
                );

                serde_json::Value::Object(properties)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_effective_from_accepts_zero() {
        assert!(validate_effective_from(0).is_ok());
    }

    #[test]
    fn validate_effective_from_accepts_valid_millis() {
        // Feb 2026 in milliseconds
        assert!(validate_effective_from(1_771_000_000_000).is_ok());
        // Jan 2000 in milliseconds
        assert!(validate_effective_from(946_684_800_000).is_ok());
        // Year 2100 in milliseconds
        assert!(validate_effective_from(4_102_444_800_000).is_ok());
    }

    #[test]
    fn validate_effective_from_rejects_nanoseconds() {
        // Feb 2026 in nanoseconds (~1.77 × 10^18)
        let nanos = 1_771_000_000_000_000_000u64;
        let result = validate_effective_from(nanos);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("nanoseconds"));
    }

    #[test]
    fn validate_effective_from_rejects_microseconds() {
        // Feb 2026 in microseconds (~1.77 × 10^15)
        let micros = 1_771_000_000_000_000u64;
        let result = validate_effective_from(micros);
        assert!(result.is_err());
    }

    #[test]
    fn validate_effective_from_accepts_boundary_value() {
        // Just under the threshold (year ~2286)
        assert!(validate_effective_from(MAX_REASONABLE_MILLIS_TIMESTAMP - 1).is_ok());
        // At the threshold
        assert!(validate_effective_from(MAX_REASONABLE_MILLIS_TIMESTAMP).is_ok());
        // Just over the threshold
        assert!(validate_effective_from(MAX_REASONABLE_MILLIS_TIMESTAMP + 1).is_err());
    }
}
