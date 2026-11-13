// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::collections::BTreeSet;

use drasi_core_types::models::{QueryJoin, QueryJoinKey};
use serde::{Deserialize, Serialize};

/// Synthetic join configuration for queries.
///
/// Joins create virtual relationships by matching property values across nodes,
/// without requiring physical relationships in the source systems. The `id`
/// names the relationship type used in the query's MATCH pattern.
///
/// ```yaml
/// joins:
///   - id: CUSTOMER
///     keys:
///       - label: Order
///         property: customer_id
///       - label: Customer
///         property: id
/// ```
///
/// This creates an edge when `Order.customer_id == Customer.id`, usable in
/// `MATCH (o:Order)-[:CUSTOMER]->(c:Customer)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryJoinConfig {
    /// Unique identifier for the join (should match relationship type in query).
    pub id: String,
    /// Keys defining the join relationship.
    pub keys: Vec<QueryJoinKeyConfig>,
}

/// One side of a synthetic join, identifying a node label and matching property.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryJoinKeyConfig {
    /// Node label to match.
    pub label: String,
    /// Property to use for joining.
    pub property: String,
}

impl From<QueryJoinKeyConfig> for QueryJoinKey {
    fn from(config: QueryJoinKeyConfig) -> Self {
        Self {
            label: config.label,
            property: config.property,
        }
    }
}

impl From<QueryJoinConfig> for QueryJoin {
    fn from(config: QueryJoinConfig) -> Self {
        Self {
            id: config.id,
            keys: config.keys.into_iter().map(Into::into).collect(),
        }
    }
}

/// Validate the same synthetic-join settings for ordinary and plugin queries.
pub fn validate_query_joins(joins: &[QueryJoinConfig]) -> anyhow::Result<()> {
    let mut ids = BTreeSet::new();
    for join in joins {
        crate::validate_identifier("join", &join.id)?;
        anyhow::ensure!(
            ids.insert(&join.id) && join.keys.len() >= 2,
            "synthetic joins require unique IDs and at least two keys"
        );
        for key in &join.keys {
            crate::validate_identifier("join label", &key.label)?;
            crate::validate_identifier("join property", &key.property)?;
        }
    }
    Ok(())
}

/// Canonicalize join/key ordering for a persisted semantic configuration.
/// Repeated keys remain significant and are not removed.
pub fn normalize_query_joins(joins: &mut [QueryJoinConfig]) {
    joins.sort_by(|left, right| left.id.cmp(&right.id));
    for join in joins {
        join.keys.sort_by(|left, right| {
            (&left.label, &left.property).cmp(&(&right.label, &right.property))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn joins() -> Vec<QueryJoinConfig> {
        serde_json::from_value(json!([
            {"id":"PLACED_BY","keys":[
                {"label":"Order","property":"customer_id"},
                {"label":"Customer","property":"id"}
            ]},
            {"id":"DELIVERED_TO","keys":[
                {"label":"Order","property":"address_id"},
                {"label":"Address","property":"id"}
            ]}
        ]))
        .expect("valid join fixture")
    }

    #[test]
    fn configuration_and_core_models_keep_the_same_serialized_shape() -> anyhow::Result<()> {
        let joins = joins();
        validate_query_joins(&joins)?;
        let core: Vec<QueryJoin> = joins.iter().cloned().map(Into::into).collect();
        assert_eq!(serde_json::to_value(&joins)?, serde_json::to_value(core)?);
        Ok(())
    }

    #[test]
    fn validation_preserves_identifier_and_membership_rules() {
        assert!(validate_query_joins(&[]).is_ok());
        let original = joins();
        let mut invalid = Vec::new();
        let mut duplicate = original.clone();
        duplicate.push(duplicate[0].clone());
        invalid.push(duplicate);
        for count in [0, 1] {
            let mut short = original.clone();
            short[0].keys.truncate(count);
            invalid.push(short);
        }
        for value in ["", "two words", "control\ncharacter"] {
            let mut id = original.clone();
            id[0].id = value.into();
            invalid.push(id);
            let mut label = original.clone();
            label[0].keys[0].label = value.into();
            invalid.push(label);
            let mut property = original.clone();
            property[0].keys[0].property = value.into();
            invalid.push(property);
        }
        for joins in invalid {
            assert!(validate_query_joins(&joins).is_err(), "{joins:?}");
        }
    }

    #[test]
    fn semantic_normalization_ignores_only_join_and_key_order() -> anyhow::Result<()> {
        let mut original = joins();
        let mut reordered = original.clone();
        reordered.reverse();
        for join in &mut reordered {
            join.keys.reverse();
        }
        normalize_query_joins(&mut original);
        normalize_query_joins(&mut reordered);
        assert_eq!(
            serde_json::to_value(&original)?,
            serde_json::to_value(&reordered)?
        );
        reordered[0].keys[0].property = "different".into();
        normalize_query_joins(&mut reordered);
        assert_ne!(
            serde_json::to_value(&original)?,
            serde_json::to_value(&reordered)?
        );
        let repeated = original[0].keys[0].clone();
        original[0].keys.push(repeated);
        normalize_query_joins(&mut original);
        assert_eq!(original[0].keys.len(), 3);
        Ok(())
    }
}
