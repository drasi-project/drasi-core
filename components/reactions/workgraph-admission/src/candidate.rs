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

//! The admission-candidate row contract.
//!
//! A row only nominates a Project Item for admission. Everything the reaction
//! actually trusts — the issue body, the item's live status, the item/issue
//! binding, and the agent profile blob — is re-read from GitHub before any
//! write, so a compromised or stale query cannot cause an unintended mutation.

use serde::{Deserialize, Serialize};

use crate::config::WorkgraphAdmissionReactionConfig;

/// One eligible Project Item + Issue pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AdmissionCandidate {
    /// `owner/repo` of the subject issue.
    pub repository: String,
    /// The subject issue number.
    pub subject_number: u64,
    /// The subject issue node ID (`I_...`).
    pub subject_node_id: String,
    /// The Project (v2) node ID (`PVT_...`).
    pub project_node_id: String,
    /// The Project (v2) item node ID (`PVTI_...`).
    pub project_item_node_id: String,
    /// The status the query observed for the item.
    pub project_status: String,
}

impl AdmissionCandidate {
    /// Reject rows that are structurally wrong or outside the allowlists.
    pub fn validate(&self, config: &WorkgraphAdmissionReactionConfig) -> anyhow::Result<()> {
        if !config.allows_repository(&self.repository) {
            anyhow::bail!(
                "repository '{}' is not in allowedRepositories",
                self.repository
            );
        }
        if self.subject_number == 0 {
            anyhow::bail!("subjectNumber must be greater than 0");
        }
        if !self.subject_node_id.starts_with("I_") {
            anyhow::bail!(
                "subjectNodeId '{}' must be a GitHub issue node ID",
                self.subject_node_id
            );
        }
        if !config.allows_project(&self.project_node_id) {
            anyhow::bail!(
                "projectNodeId '{}' is not in allowedProjects",
                self.project_node_id
            );
        }
        if !self.project_item_node_id.starts_with("PVTI_") {
            anyhow::bail!(
                "projectItemNodeId '{}' must be a Project v2 item node ID",
                self.project_item_node_id
            );
        }
        if self.project_status != config.expected_source_status {
            anyhow::bail!(
                "projectStatus '{}' is not the admission source status '{}'",
                self.project_status,
                config.expected_source_status
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> WorkgraphAdmissionReactionConfig {
        WorkgraphAdmissionReactionConfig {
            allowed_repositories: vec!["drasi-project/drasi-core".to_string()],
            allowed_projects: vec!["PVT_project".to_string()],
            expected_project_status_field_node_id: "PVTSSF_status".to_string(),
            expected_source_status: "Triage".to_string(),
            trusted_author_database_id: 4021243,
            trusted_author_type: drasi_workgraph_common::trust::ActorType::Bot,
            ..WorkgraphAdmissionReactionConfig::default()
        }
    }

    fn candidate() -> AdmissionCandidate {
        AdmissionCandidate {
            repository: "drasi-project/drasi-core".to_string(),
            subject_number: 742,
            subject_node_id: "I_kwDOABCDEF6ABCDE".to_string(),
            project_node_id: "PVT_project".to_string(),
            project_item_node_id: "PVTI_item".to_string(),
            project_status: "Triage".to_string(),
        }
    }

    #[test]
    fn valid_candidate_passes() {
        candidate().validate(&config()).expect("valid candidate");
    }

    #[test]
    fn allowlists_are_enforced() {
        let mut row = candidate();
        row.repository = "attacker/repo".to_string();
        assert!(row
            .validate(&config())
            .expect_err("repo allowlist")
            .to_string()
            .contains("allowedRepositories"));

        let mut row = candidate();
        row.project_node_id = "PVT_other".to_string();
        assert!(row
            .validate(&config())
            .expect_err("project allowlist")
            .to_string()
            .contains("allowedProjects"));
    }

    #[test]
    fn node_id_shapes_are_enforced() {
        let mut row = candidate();
        row.subject_node_id = "PR_notanissue".to_string();
        assert!(row.validate(&config()).is_err());

        let mut row = candidate();
        row.project_item_node_id = "PVT_notanitem".to_string();
        assert!(row.validate(&config()).is_err());
    }

    #[test]
    fn only_the_configured_source_status_is_admissible() {
        let mut row = candidate();
        row.project_status = "AwaitingValidation".to_string();
        assert!(row
            .validate(&config())
            .expect_err("wrong source status")
            .to_string()
            .contains("admission source status"));
    }

    #[test]
    fn unknown_row_fields_are_rejected() {
        let error = serde_json::from_value::<AdmissionCandidate>(serde_json::json!({
            "repository": "o/r",
            "subjectNumber": 1,
            "subjectNodeId": "I_1",
            "projectNodeId": "PVT_1",
            "projectItemNodeId": "PVTI_1",
            "projectStatus": "Triage",
            "actor": "mallory"
        }))
        .expect_err("unknown row field");
        assert!(error.to_string().contains("actor"));
    }
}
