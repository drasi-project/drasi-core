// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{fmt, sync::Arc};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{ContractError, Result};

/// Immutable UUID shared by a component instance and its computation-graph node.
///
/// Possessing an ID does not authorize adding a component with that identity.
/// The owning DrasiLib applies its creation policy separately.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ComponentId {
    uuid: Uuid,
    text: Arc<str>,
}

impl ComponentId {
    /// Allocate an identity for a new component, independently of its name.
    pub fn generate() -> Self {
        Self::from_uuid(Uuid::new_v4())
    }

    /// Preserve a supplied UUID without assigning a different identity.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self {
            uuid,
            text: uuid.hyphenated().to_string().into(),
        }
    }

    /// Identify a framework-owned helper from its owner's UUID and a fixed key.
    ///
    /// The key describes the helper's purpose, never the owner's editable name.
    /// This does not grant permission to add the helper to a DrasiLib instance.
    pub fn derive_owned(owner: &Self, key: &str) -> Result<Self> {
        crate::validate_identifier("component helper key", key)?;
        let mut hash = Sha256::new();
        hash.update(b"drasi.component-helper-id.v2");
        hash.update(owner.as_uuid().as_bytes());
        hash.update((key.len() as u64).to_be_bytes());
        hash.update(key.as_bytes());
        let digest = hash.finalize();
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Ok(Self::from_uuid(Uuid::from_bytes(bytes)))
    }

    /// Parse a UUID. Component names and old string IDs are not accepted.
    pub fn try_new(value: impl Into<Arc<str>>) -> Result<Self> {
        let value = value.into();
        let uuid = Uuid::parse_str(&value).map_err(|source| ContractError::InvalidComponentId {
            value: value.to_string(),
            source,
        })?;
        Ok(Self::from_uuid(uuid))
    }

    /// The UUID value, independent of its textual representation.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.uuid
    }

    /// Canonical lowercase, hyphenated UUID text.
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl From<Uuid> for ComponentId {
    fn from(uuid: Uuid) -> Self {
        Self::from_uuid(uuid)
    }
}

impl fmt::Display for ComponentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for ComponentId {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ComponentId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::try_new(value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashSet};

    use super::*;
    use crate::ComponentName;

    const TEXT: &str = "00112233-4455-4677-8899-aabbccddeeff";

    #[test]
    fn generated_component_ids_are_distinct_uuids() {
        let first = ComponentId::generate();
        let second = ComponentId::generate();
        assert_ne!(first, second);
        assert_eq!(first.as_uuid().get_version_num(), 4);
        assert_eq!(Uuid::parse_str(first.as_str()).unwrap(), *first.as_uuid());
    }

    #[test]
    fn supplied_uuid_is_preserved_and_text_is_canonical() {
        let uuid = Uuid::parse_str(TEXT).unwrap();
        let parsed = ComponentId::try_new(TEXT.to_ascii_uppercase()).unwrap();
        let supplied = ComponentId::from_uuid(uuid);
        assert_eq!(parsed, supplied);
        assert_eq!(parsed.as_uuid(), &uuid);
        assert_eq!(parsed.as_str(), TEXT);
        assert_eq!(parsed.to_string(), TEXT);
        assert_eq!(HashSet::from([parsed.clone(), supplied.clone()]).len(), 1);
        assert_eq!(BTreeSet::from([parsed, supplied]).len(), 1);
    }

    #[test]
    fn component_names_are_not_accepted_as_ids() {
        for value in ["orders", "source/reader", "", "not-a-uuid"] {
            assert!(matches!(
                ComponentId::try_new(value),
                Err(ContractError::InvalidComponentId { .. })
            ));
            assert!(serde_json::from_value::<ComponentId>(serde_json::json!(value)).is_err());
        }
        for value in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!(17),
        ] {
            assert!(serde_json::from_value::<ComponentId>(value).is_err());
        }
    }

    #[test]
    fn clones_and_serialization_preserve_the_same_component_uuid() {
        let id = ComponentId::generate();
        assert_eq!(id.clone(), id);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<ComponentId>(&json).unwrap(), id);
        assert_eq!(
            rmp_serde::from_slice::<ComponentId>(&rmp_serde::to_vec(&id).unwrap()).unwrap(),
            id
        );
        assert_eq!(
            bincode::deserialize::<ComponentId>(&bincode::serialize(&id).unwrap()).unwrap(),
            id
        );
    }

    #[test]
    fn component_names_have_a_separate_case_sensitive_type() {
        let name = ComponentName::try_new("orders").unwrap();
        assert_eq!(name.as_str(), "orders");
        assert_ne!(name, ComponentName::try_new("Orders").unwrap());
        assert_eq!(
            serde_json::from_str::<ComponentName>("\"orders\"").unwrap(),
            name
        );
        for value in ["", "invalid name", "name\n"] {
            assert!(ComponentName::try_new(value).is_err());
        }
    }

    #[test]
    fn owned_helpers_are_stable_and_distinct_by_owner_and_purpose() {
        let owner = ComponentId::try_new(TEXT).unwrap();
        let helper = ComponentId::derive_owned(&owner, "scheduled-input").unwrap();
        assert_eq!(
            helper,
            ComponentId::derive_owned(&owner.clone(), "scheduled-input").unwrap()
        );
        assert_eq!(helper.as_uuid().get_version_num(), 8);
        assert_ne!(helper, owner);
        assert_ne!(
            helper,
            ComponentId::derive_owned(&owner, "results-outlet").unwrap()
        );
        assert_ne!(
            helper,
            ComponentId::derive_owned(&ComponentId::generate(), "scheduled-input").unwrap()
        );
        assert!(ComponentId::derive_owned(&owner, "").is_err());
    }
}
