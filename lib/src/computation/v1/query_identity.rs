// Copyright 2026 The Drasi Authors.
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

use serde::{Deserialize, Serialize};

use super::{ChangeEnvelope, ComponentId, ContextEntry, ContextValue, ContractError};

const IDENTITY: &str = "drasi.query-recovery-identity.v1";

/// The construction/instance scope, local graph/query identity and configuration,
/// plus the volatile publication lifetime. Persistent queries omit the incarnation
/// and keep their identity when the same scoped configuration and stores reopen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryRecoveryIdentity {
    construction_scope: String,
    graph_id: String,
    query_id: ComponentId,
    configuration_hash: u64,
    #[serde(deserialize_with = "required_incarnation")]
    incarnation: Option<uuid::Uuid>,
}

fn required_incarnation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<uuid::Uuid>, D::Error> {
    Option::<uuid::Uuid>::deserialize(deserializer)
}

#[derive(Debug, thiserror::Error)]
pub enum QueryIdentityError {
    #[error("query output omitted its producer recovery identity")]
    Missing,
    #[error("invalid query producer recovery identity: {0}")]
    Invalid(String),
    #[error("unsupported query producer identity version {0}")]
    UnsupportedVersion(u32),
    #[error("query output belongs to a different producer recovery identity")]
    Mismatch,
    #[error(transparent)]
    Contract(#[from] ContractError),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityRecord {
    version: u32,
    identity: QueryRecoveryIdentity,
}

impl QueryRecoveryIdentity {
    pub fn try_new(
        graph_id: impl Into<String>,
        query_id: ComponentId,
        configuration_hash: u64,
        incarnation: Option<uuid::Uuid>,
    ) -> Result<Self, QueryIdentityError> {
        let graph_id = graph_id.into();
        let identity = Self {
            construction_scope: graph_id.clone(),
            graph_id,
            query_id,
            configuration_hash,
            incarnation,
        };
        identity.validate()?;
        Ok(identity)
    }

    pub fn graph_id(&self) -> &str {
        &self.graph_id
    }

    pub fn construction_scope(&self) -> &str {
        &self.construction_scope
    }

    pub(crate) fn with_construction_scope(
        mut self,
        scope: &str,
    ) -> Result<Self, QueryIdentityError> {
        self.construction_scope = scope.to_owned();
        self.validate()?;
        Ok(self)
    }

    pub fn query_id(&self) -> &str {
        self.query_id.as_str()
    }

    pub fn configuration_hash(&self) -> u64 {
        self.configuration_hash
    }

    pub fn incarnation(&self) -> Option<uuid::Uuid> {
        self.incarnation
    }

    pub(crate) fn validate(&self) -> Result<(), QueryIdentityError> {
        super::data::validate_identifier("query construction scope", &self.construction_scope)?;
        super::data::validate_identifier("graph", &self.graph_id)?;
        super::data::validate_identifier("query", self.query_id.as_str())?;
        if self.incarnation.is_some_and(|value| value.is_nil()) {
            return Err(QueryIdentityError::Invalid(
                "nil volatile incarnation".into(),
            ));
        }
        Ok(())
    }

    pub fn annotate(
        &self,
        envelope: &mut ChangeEnvelope,
        component: &ComponentId,
    ) -> Result<(), QueryIdentityError> {
        self.validate()?;
        let bytes = serde_json::to_vec(&IdentityRecord {
            version: 1,
            identity: self.clone(),
        })
        .map_err(|error| QueryIdentityError::Invalid(error.to_string()))?;
        envelope.append_annotation(ContextEntry::try_new(
            component.clone(),
            IDENTITY,
            ContextValue::Bytes(Arc::from(bytes)),
        )?)?;
        Ok(())
    }

    pub fn from_envelope(envelope: &ChangeEnvelope) -> Result<Self, QueryIdentityError> {
        Self::optional_from_envelope(envelope)?.ok_or(QueryIdentityError::Missing)
    }

    pub(crate) fn optional_from_envelope(
        envelope: &ChangeEnvelope,
    ) -> Result<Option<Self>, QueryIdentityError> {
        let Some(entry) = envelope
            .annotations()
            .entries()
            .find(|entry| entry.key() == IDENTITY)
        else {
            return Ok(None);
        };
        let ContextValue::Bytes(bytes) = entry.value() else {
            return Err(QueryIdentityError::Invalid(
                "annotation is not bytes".into(),
            ));
        };
        let record: IdentityRecord = serde_json::from_slice(&bytes)
            .map_err(|error| QueryIdentityError::Invalid(error.to_string()))?;
        if record.version != 1 {
            return Err(QueryIdentityError::UnsupportedVersion(record.version));
        }
        record.identity.validate()?;
        Ok(Some(record.identity))
    }
}
