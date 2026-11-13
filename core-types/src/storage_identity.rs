// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Logical storage domains. Backend encoders, not component APIs, turn these
//! values into physical keys.

use serde::{Deserialize, Serialize};

use crate::{ComponentId, StreamId};

/// A physical namespace, independent of the component that owns stored data.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StorageScope(Vec<String>);

impl StorageScope {
    pub fn new(namespace: impl Into<String>) -> Self {
        Self(vec![namespace.into()])
    }

    pub fn segments(&self) -> &[String] {
        &self.0
    }

    pub fn nested(&self, inner: &Self) -> Self {
        Self(self.0.iter().chain(&inner.0).cloned().collect())
    }

    pub fn storage_key(&self) -> String {
        let mut key = String::from("drasi_scope_v1");
        for segment in &self.0 {
            key.push('_');
            key.push_str(&segment.len().to_string());
            key.push('_');
            key.push_str(&hex(segment));
        }
        key
    }

    pub fn component_key(&self, id: &ComponentId) -> String {
        if self.0.is_empty() {
            id.to_string()
        } else {
            format!("{}_{id}", self.storage_key())
        }
    }
}

/// A component's state or a separately named, non-component store.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum StorePartition {
    Component {
        scope: StorageScope,
        id: ComponentId,
    },
    Named {
        scope: StorageScope,
        key: String,
    },
}

impl StorePartition {
    pub fn component(id: ComponentId) -> Self {
        Self::Component {
            scope: StorageScope::default(),
            id,
        }
    }

    pub fn named(key: impl Into<String>) -> Self {
        Self::Named {
            scope: StorageScope::default(),
            key: key.into(),
        }
    }

    pub fn in_scope(&self, outer: &StorageScope) -> Self {
        match self {
            Self::Component { scope, id } => Self::Component {
                scope: outer.nested(scope),
                id: id.clone(),
            },
            Self::Named { scope, key } => Self::Named {
                scope: outer.nested(scope),
                key: key.clone(),
            },
        }
    }

    pub fn storage_key(&self) -> String {
        match self {
            Self::Component { scope, id } => scope.component_key(id),
            Self::Named { scope, key } => {
                format!(
                    "drasi_named_store_v1_{}_{}",
                    hex(&scope.storage_key()),
                    hex(key)
                )
            }
        }
    }
}

/// Logical output ownership, including journals that are not components.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum OutputPartition {
    Component(ComponentId),
    Journal(String),
    Metadata {
        output: Box<OutputPartition>,
        key: String,
    },
    Group {
        journal: Option<String>,
        partition: Box<OutputPartition>,
    },
}

impl OutputPartition {
    pub fn validate(&self) -> Result<(), super::interface::IndexError> {
        let valid_key = |key: &str| {
            if key.is_empty() || key.contains('\0') {
                Err(super::interface::IndexError::other(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "journal identity must be nonempty and contain no NUL",
                )))
            } else {
                Ok(())
            }
        };
        match self {
            Self::Component(_) => Ok(()),
            Self::Journal(key) => valid_key(key),
            Self::Metadata { output, key } => {
                valid_key(key)?;
                output.validate()
            }
            Self::Group { journal, partition } => {
                if let Some(journal) = journal {
                    valid_key(journal)?;
                }
                partition.validate()
            }
        }
    }

    pub fn in_group(&self, journal: Option<&str>) -> Self {
        Self::Group {
            journal: journal.map(str::to_owned),
            partition: Box::new(self.clone()),
        }
    }

    pub fn metadata(&self, key: impl Into<String>) -> Self {
        Self::Metadata {
            output: Box::new(self.clone()),
            key: key.into(),
        }
    }

    pub fn storage_key(&self) -> String {
        match self {
            Self::Component(id) => id.to_string(),
            Self::Journal(key) => format!("drasi_journal_v1_{}", hex(key)),
            Self::Metadata { output, key } => format!(
                "drasi_output_metadata_v1_{}_{}",
                hex(&output.storage_key()),
                hex(key)
            ),
            Self::Group { journal, partition } => match journal {
                Some(journal) => format!(
                    "drasi_group_v1_journal_{}_{}",
                    hex(journal),
                    hex(&partition.storage_key())
                ),
                None => format!("drasi_group_v1_processor_{}", hex(&partition.storage_key())),
            },
        }
    }
}

/// Source/stream cursors and framework state occupy disjoint key spaces.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CheckpointKey {
    Source(ComponentId),
    Stream(StreamId),
    Metadata(String),
    Associated {
        checkpoint: Box<CheckpointKey>,
        name: String,
    },
    Journal {
        journal: OutputPartition,
        key: String,
    },
}

impl CheckpointKey {
    pub fn is_progress(&self) -> bool {
        matches!(self, Self::Source(_) | Self::Stream(_))
    }

    pub fn associated(&self, name: impl Into<String>) -> Self {
        Self::Associated {
            checkpoint: Box::new(self.clone()),
            name: name.into(),
        }
    }

    pub fn associated_checkpoint(&self, name: &str) -> Option<&Self> {
        match self {
            Self::Associated {
                checkpoint,
                name: actual,
            } if actual == name => Some(checkpoint),
            _ => None,
        }
    }

    pub fn metadata_name(&self) -> Option<&str> {
        match self {
            Self::Metadata(name) => Some(name),
            _ => None,
        }
    }

    pub fn storage_key(&self) -> Result<String, serde_json::Error> {
        match self {
            Self::Source(id) => Ok(id.to_string()),
            _ => Ok(format!(
                "\0drasi-checkpoint:v2:{}",
                serde_json::to_string(self)?
            )),
        }
    }

    pub fn from_storage_key(key: &str) -> Result<Self, super::interface::IndexError> {
        if let Some(value) = key.strip_prefix("\0drasi-checkpoint:v2:") {
            let key: Self =
                serde_json::from_str(value).map_err(super::interface::IndexError::other)?;
            if matches!(key, Self::Source(_)) {
                return Err(super::interface::IndexError::CorruptedData);
            }
            Ok(key)
        } else {
            ComponentId::try_new(key)
                .map(Self::Source)
                .map_err(super::interface::IndexError::other)
        }
    }
}

fn hex(value: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.bytes() {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> ComponentId {
        ComponentId::from_uuid(uuid::Uuid::from_u128(1))
    }

    #[test]
    fn scope_segments_are_structural_and_path_safe() {
        let split = StorageScope::new("one").nested(&StorageScope::new("two"));
        let combined = StorageScope::new("one/two");
        assert_ne!(split.component_key(&id()), combined.component_key(&id()));
        assert_ne!(StorageScope::default(), StorageScope::new(""));
        let key = StorageScope::new("../one\\two\0").component_key(&id());
        assert!(!key.contains(['/', '\\', '\0']));
        assert_eq!(StorageScope::default().component_key(&id()), id().as_str());
    }

    #[test]
    fn named_stores_do_not_alias_components_or_other_scopes() {
        let component = StorePartition::component(id());
        let named = StorePartition::named(id().to_string());
        assert_ne!(component.storage_key(), named.storage_key());
        let scope = StorageScope::new("instance");
        assert_ne!(
            component.storage_key(),
            component.in_scope(&scope).storage_key()
        );
        assert_ne!(
            component.in_scope(&scope).storage_key(),
            named.in_scope(&scope).storage_key()
        );
        assert_eq!(
            component
                .in_scope(&StorageScope::new("inner"))
                .in_scope(&scope),
            component.in_scope(&scope.nested(&StorageScope::new("inner"))),
        );
    }

    #[test]
    fn output_owners_journals_metadata_and_groups_are_disjoint() {
        let component = OutputPartition::Component(id());
        let journal = OutputPartition::Journal(id().to_string());
        let keys = std::collections::BTreeSet::from([
            component.storage_key(),
            journal.storage_key(),
            component.metadata("state").storage_key(),
            journal.metadata("state").storage_key(),
            component.in_group(None).storage_key(),
            component.in_group(Some("journal")).storage_key(),
        ]);
        assert_eq!(keys.len(), 6);
    }

    #[test]
    fn checkpoint_domains_round_trip_without_aliasing() {
        let keys = [
            CheckpointKey::Source(id()),
            CheckpointKey::Stream(StreamId::try_new(id().as_str()).unwrap()),
            CheckpointKey::Metadata(id().to_string()),
            CheckpointKey::Source(id()).associated("position"),
            CheckpointKey::Metadata("\0bootstrap".into()),
            CheckpointKey::Journal {
                journal: OutputPartition::Component(id()),
                key: "cursor".into(),
            },
        ];
        let mut encoded = std::collections::BTreeSet::new();
        for key in keys {
            let bytes = key.storage_key().unwrap();
            assert!(encoded.insert(bytes.clone()));
            assert_eq!(CheckpointKey::from_storage_key(&bytes).unwrap(), key);
        }
    }

    #[test]
    fn checkpoint_decoding_rejects_names_and_alternate_source_encodings() {
        for key in [
            "orders",
            "\0drasi-checkpoint:v1:orders",
            "\0drasi-checkpoint:v2:invalid",
        ] {
            assert!(CheckpointKey::from_storage_key(key).is_err());
        }
        let alternate = format!(
            "\0drasi-checkpoint:v2:{}",
            serde_json::to_string(&CheckpointKey::Source(id())).unwrap()
        );
        assert!(CheckpointKey::from_storage_key(&alternate).is_err());
    }
}
