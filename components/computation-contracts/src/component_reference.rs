// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{fmt, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::{ComponentId, ComponentName, Result};

/// An explicit component selector used before a graph resolves a connection.
///
/// A string always selects a name, even when it resembles a UUID. ID references
/// use an object containing only `id`. Resolution and persistence of that
/// selection belong to the owning graph, not to this data contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ComponentReference {
    Name(ComponentName),
    Id { id: ComponentId },
}

impl ComponentReference {
    /// Select a component by its current unique name.
    pub fn by_name(name: impl Into<Arc<str>>) -> Result<Self> {
        Ok(Self::Name(ComponentName::try_new(name)?))
    }

    /// Select the component identity, independently of its current name.
    pub fn by_id(id: ComponentId) -> Self {
        Self::Id { id }
    }

    pub fn as_name(&self) -> Option<&ComponentName> {
        match self {
            Self::Name(name) => Some(name),
            Self::Id { .. } => None,
        }
    }

    pub fn as_id(&self) -> Option<&ComponentId> {
        match self {
            Self::Id { id } => Some(id),
            Self::Name(_) => None,
        }
    }
}

impl fmt::Display for ComponentReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name(name) => write!(formatter, "component named '{name}'"),
            Self::Id { id } => write!(formatter, "component {id}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "00112233-4455-4677-8899-aabbccddeeff";

    #[test]
    fn component_reference_strings_are_names_even_when_uuid_shaped() {
        for value in ["orders", ID] {
            let reference: ComponentReference =
                serde_json::from_value(serde_json::json!(value)).unwrap();
            assert_eq!(reference.as_name().unwrap().as_str(), value);
            assert!(reference.as_id().is_none());
            assert_eq!(
                serde_json::to_value(&reference).unwrap(),
                serde_json::json!(value)
            );
        }
    }

    #[test]
    fn component_reference_uuid_selector_has_an_explicit_shape() {
        let reference: ComponentReference =
            serde_json::from_value(serde_json::json!({"id": ID})).unwrap();
        assert_eq!(reference.as_id(), Some(&ComponentId::try_new(ID).unwrap()));
        assert!(reference.as_name().is_none());
        assert_eq!(
            serde_json::to_value(&reference).unwrap(),
            serde_json::json!({"id": ID})
        );
    }

    #[test]
    fn component_reference_rejects_malformed_or_ambiguous_selectors() {
        for value in [
            serde_json::json!({"id": "orders"}),
            serde_json::json!({"id": ID, "name": "orders"}),
            serde_json::json!({"id": ID, "extra": true}),
            serde_json::json!({"name": "orders"}),
            serde_json::json!({}),
            serde_json::json!(""),
            serde_json::json!("invalid name"),
            serde_json::Value::Null,
        ] {
            assert!(
                serde_json::from_value::<ComponentReference>(value.clone()).is_err(),
                "accepted invalid component reference: {value}"
            );
        }
    }
}
