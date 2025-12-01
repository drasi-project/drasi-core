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

//! Bootstrap script record type definitions
//!
//! This module defines the type hierarchy for bootstrap script records used in JSONL script files.
//! Records are serialized as JSON Lines (one JSON object per line) with a "kind" field that
//! discriminates between different record types.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

/// Main enum representing all possible bootstrap script record types
///
/// Uses tagged serialization with "kind" field to discriminate variants.
/// Example: `{"kind":"Node","id":"n1","labels":["Person"],"properties":{"name":"Alice"}}`
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum BootstrapScriptRecord {
    /// Comment record (filtered out during processing)
    Comment(CommentRecord),
    /// Header record (required as first record in script)
    Header(BootstrapHeaderRecord),
    /// Label/checkpoint record for script navigation
    Label(LabelRecord),
    /// Node record representing a graph node/entity
    Node(NodeRecord),
    /// Relation record representing a graph edge/relationship
    Relation(RelationRecord),
    /// Finish record marking end of script
    Finish(BootstrapFinishRecord),
}

/// Header record that must appear first in every bootstrap script
///
/// Provides metadata about the script including start time and description.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct BootstrapHeaderRecord {
    /// Script start time (used as reference for offset calculations)
    pub start_time: DateTime<FixedOffset>,
    /// Human-readable description of the script
    #[serde(default)]
    pub description: String,
}

/// Node record representing a graph node/entity
///
/// Contains node identity, labels, and properties as arbitrary JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeRecord {
    /// Unique identifier for the node
    pub id: String,
    /// List of labels/types for the node
    pub labels: Vec<String>,
    /// Node properties as arbitrary JSON value
    #[serde(default)]
    pub properties: serde_json::Value,
}

/// Relation record representing a graph edge/relationship
///
/// Contains relationship identity, labels, start/end node references, and properties.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RelationRecord {
    /// Unique identifier for the relation
    pub id: String,
    /// List of labels/types for the relation
    pub labels: Vec<String>,
    /// ID of the start/source node
    pub start_id: String,
    /// Optional label of the start node
    pub start_label: Option<String>,
    /// ID of the end/target node
    pub end_id: String,
    /// Optional label of the end node
    pub end_label: Option<String>,
    /// Relation properties as arbitrary JSON value
    #[serde(default)]
    pub properties: serde_json::Value,
}

/// Label/checkpoint record for marking positions in the script
///
/// Allows navigation to specific points in the script timeline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LabelRecord {
    /// Nanoseconds offset from script start time
    #[serde(default)]
    pub offset_ns: u64,
    /// Label name for this checkpoint
    pub label: String,
    /// Description of this checkpoint
    #[serde(default)]
    pub description: String,
}

/// Comment record for documentation within scripts
///
/// These records are automatically filtered out during processing and never
/// returned to consumers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommentRecord {
    /// Comment text
    pub comment: String,
}

/// Finish record marking the end of a bootstrap script
///
/// If not present in the script file, one is automatically generated.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BootstrapFinishRecord {
    /// Description of script completion
    #[serde(default)]
    pub description: String,
}

/// Wrapper that adds sequence numbering to bootstrap script records
///
/// Used by the reader to track the order in which records are processed,
/// especially when reading from multiple files.
#[derive(Clone, Debug, Serialize)]
pub struct SequencedBootstrapScriptRecord {
    /// Sequence number (order in which record was read)
    pub seq: u64,
    /// The actual record
    pub record: BootstrapScriptRecord,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_header_record_serialization() {
        let header = BootstrapHeaderRecord {
            start_time: DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z").unwrap(),
            description: "Test Script".to_string(),
        };
        let record = BootstrapScriptRecord::Header(header);

        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""kind":"Header"#));
        assert!(json.contains(r#""description":"Test Script"#));

        // Test deserialization
        let deserialized: BootstrapScriptRecord = serde_json::from_str(&json).unwrap();
        if let BootstrapScriptRecord::Header(h) = deserialized {
            assert_eq!(h.description, "Test Script");
        } else {
            panic!("Expected Header record");
        }
    }

    #[test]
    fn test_node_record_serialization() {
        let node = NodeRecord {
            id: "n1".to_string(),
            labels: vec!["Person".to_string()],
            properties: json!({"name": "Alice", "age": 30}),
        };
        let record = BootstrapScriptRecord::Node(node);

        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""kind":"Node"#));
        assert!(json.contains(r#""id":"n1"#));

        // Test deserialization
        let deserialized: BootstrapScriptRecord = serde_json::from_str(&json).unwrap();
        if let BootstrapScriptRecord::Node(n) = deserialized {
            assert_eq!(n.id, "n1");
            assert_eq!(n.labels, vec!["Person"]);
        } else {
            panic!("Expected Node record");
        }
    }

    #[test]
    fn test_relation_record_serialization() {
        let relation = RelationRecord {
            id: "r1".to_string(),
            labels: vec!["KNOWS".to_string()],
            start_id: "n1".to_string(),
            start_label: Some("Person".to_string()),
            end_id: "n2".to_string(),
            end_label: Some("Person".to_string()),
            properties: json!({"since": 2020}),
        };
        let record = BootstrapScriptRecord::Relation(relation);

        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""kind":"Relation"#));
        assert!(json.contains(r#""start_id":"n1"#));

        // Test deserialization
        let deserialized: BootstrapScriptRecord = serde_json::from_str(&json).unwrap();
        if let BootstrapScriptRecord::Relation(r) = deserialized {
            assert_eq!(r.id, "r1");
            assert_eq!(r.start_id, "n1");
            assert_eq!(r.end_id, "n2");
        } else {
            panic!("Expected Relation record");
        }
    }

    #[test]
    fn test_comment_record_serialization() {
        let comment = CommentRecord {
            comment: "This is a comment".to_string(),
        };
        let record = BootstrapScriptRecord::Comment(comment);

        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""kind":"Comment"#));

        // Test deserialization
        let deserialized: BootstrapScriptRecord = serde_json::from_str(&json).unwrap();
        if let BootstrapScriptRecord::Comment(c) = deserialized {
            assert_eq!(c.comment, "This is a comment");
        } else {
            panic!("Expected Comment record");
        }
    }

    #[test]
    fn test_label_record_serialization() {
        let label = LabelRecord {
            offset_ns: 1000,
            label: "checkpoint_1".to_string(),
            description: "First checkpoint".to_string(),
        };
        let record = BootstrapScriptRecord::Label(label);

        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""kind":"Label"#));

        // Test deserialization
        let deserialized: BootstrapScriptRecord = serde_json::from_str(&json).unwrap();
        if let BootstrapScriptRecord::Label(l) = deserialized {
            assert_eq!(l.label, "checkpoint_1");
            assert_eq!(l.offset_ns, 1000);
        } else {
            panic!("Expected Label record");
        }
    }

    #[test]
    fn test_finish_record_serialization() {
        let finish = BootstrapFinishRecord {
            description: "End of script".to_string(),
        };
        let record = BootstrapScriptRecord::Finish(finish);

        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""kind":"Finish"#));

        // Test deserialization
        let deserialized: BootstrapScriptRecord = serde_json::from_str(&json).unwrap();
        if let BootstrapScriptRecord::Finish(f) = deserialized {
            assert_eq!(f.description, "End of script");
        } else {
            panic!("Expected Finish record");
        }
    }

    #[test]
    fn test_default_header_record() {
        let header = BootstrapHeaderRecord::default();
        assert!(header.description.is_empty());
    }

    #[test]
    fn test_node_with_empty_properties() {
        let json = r#"{"kind":"Node","id":"n1","labels":["Test"]}"#;
        let record: BootstrapScriptRecord = serde_json::from_str(json).unwrap();

        if let BootstrapScriptRecord::Node(n) = record {
            assert_eq!(n.id, "n1");
            assert_eq!(n.properties, serde_json::Value::Null);
        } else {
            panic!("Expected Node record");
        }
    }

    #[test]
    fn test_relation_without_optional_labels() {
        let json =
            r#"{"kind":"Relation","id":"r1","labels":["REL"],"start_id":"n1","end_id":"n2"}"#;
        let record: BootstrapScriptRecord = serde_json::from_str(json).unwrap();

        if let BootstrapScriptRecord::Relation(r) = record {
            assert_eq!(r.start_label, None);
            assert_eq!(r.end_label, None);
        } else {
            panic!("Expected Relation record");
        }
    }
}
