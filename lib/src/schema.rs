// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

pub use drasi_core_types::schema::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_schema_is_empty() {
        let schema = SourceSchema::default();
        assert!(schema.is_empty());

        let schema = SourceSchema {
            nodes: vec![NodeSchema::new("User")],
            relations: Vec::new(),
        };
        assert!(!schema.is_empty());
    }

    #[test]
    fn test_normalize_table_label_strips_schema() {
        assert_eq!(normalize_table_label("public.users"), "users");
        assert_eq!(normalize_table_label("dbo.orders"), "orders");
        assert_eq!(normalize_table_label("orders"), "orders");
    }

    #[test]
    fn test_merge_source_schema_nodes() {
        let mut graph = GraphSchema::default();
        let source_schema = SourceSchema {
            nodes: vec![NodeSchema {
                label: "User".to_string(),
                properties: vec![PropertySchema {
                    name: "name".to_string(),
                    data_type: Some(PropertyType::String),
                    description: None,
                }],
            }],
            relations: Vec::new(),
        };

        graph.merge_source_schema("source1", &source_schema);

        assert!(graph.nodes.contains_key("User"));
        let node = &graph.nodes["User"];
        assert!(node.sources.contains("source1"));
        assert_eq!(node.properties.len(), 1);
        assert_eq!(node.properties[0].name, "name");
    }

    #[test]
    fn test_merge_source_schema_deduplicates_properties() {
        let mut graph = GraphSchema::default();
        let schema1 = SourceSchema {
            nodes: vec![NodeSchema {
                label: "User".to_string(),
                properties: vec![PropertySchema {
                    name: "age".to_string(),
                    data_type: None,
                    description: None,
                }],
            }],
            relations: Vec::new(),
        };
        let schema2 = SourceSchema {
            nodes: vec![NodeSchema {
                label: "User".to_string(),
                properties: vec![PropertySchema {
                    name: "age".to_string(),
                    data_type: Some(PropertyType::Integer),
                    description: Some("User age".to_string()),
                }],
            }],
            relations: Vec::new(),
        };

        graph.merge_source_schema("s1", &schema1);
        graph.merge_source_schema("s2", &schema2);

        let node = &graph.nodes["User"];
        assert_eq!(node.properties.len(), 1);
        // Second merge promotes type
        assert_eq!(node.properties[0].data_type, Some(PropertyType::Integer));
        assert_eq!(node.properties[0].description, Some("User age".to_string()));
    }

    #[test]
    fn test_merge_source_schema_relations() {
        let mut graph = GraphSchema::default();
        let schema = SourceSchema {
            nodes: Vec::new(),
            relations: vec![RelationSchema {
                label: "KNOWS".to_string(),
                from: Some("User".to_string()),
                to: Some("User".to_string()),
                properties: Vec::new(),
            }],
        };

        graph.merge_source_schema("s1", &schema);

        let rel = &graph.relations["KNOWS"];
        assert_eq!(rel.from, Some("User".to_string()));
        assert_eq!(rel.to, Some("User".to_string()));
    }

    #[test]
    fn test_mark_queried_nodes() {
        let mut graph = GraphSchema::default();
        graph.mark_queried_nodes(["User", "Order"].iter().copied(), "q1");

        assert!(graph.nodes["User"].queried_by.contains("q1"));
        assert!(graph.nodes["Order"].queried_by.contains("q1"));
    }

    #[test]
    fn test_mark_queried_relations() {
        let mut graph = GraphSchema::default();
        graph.mark_queried_relations(["PLACED"].iter().copied(), "q1");

        assert!(graph.relations["PLACED"].queried_by.contains("q1"));
    }

    #[test]
    fn test_record_source_without_schema() {
        let mut graph = GraphSchema::default();
        graph.record_source_without_schema("unknown-source");
        assert!(graph.sources_without_schema.contains("unknown-source"));
    }

    #[test]
    fn test_source_schema_serialization_roundtrip() {
        let schema = SourceSchema {
            nodes: vec![NodeSchema {
                label: "Sensor".to_string(),
                properties: vec![PropertySchema {
                    name: "temperature".to_string(),
                    data_type: Some(PropertyType::Float),
                    description: Some("Celsius".to_string()),
                }],
            }],
            relations: vec![RelationSchema {
                label: "MEASURES".to_string(),
                from: Some("Sensor".to_string()),
                to: Some("Location".to_string()),
                properties: Vec::new(),
            }],
        };

        let json = serde_json::to_string(&schema).unwrap();
        let deserialized: SourceSchema = serde_json::from_str(&json).unwrap();
        assert_eq!(schema, deserialized);
    }
}
