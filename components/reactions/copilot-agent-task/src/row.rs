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

//! The launch-query row schema and its allowlist validation.
//!
//! The reaction subscribes to a single "launch query" and acts **only** on
//! `Add` result diffs (see [`crate::reaction`]). Each added row merely
//! *nominates* a run: everything the reaction trusts (issue body, project
//! status, the assignment, the pinned profile blob) is re-read from GitHub
//! before any write. The row therefore carries only the identifiers needed to
//! locate that authoritative state plus the model policy for the task.
//!
//! Anything that does not match the exact schema below — an unknown field, a
//! malformed node ID, or a disallowed repository/model — makes the row a
//! permanent rejection (fail-closed: the reaction never launches on malformed
//! or disallowed input).

use anyhow::{bail, Context, Result};
use drasi_workgraph_common::event::RunId;
use serde::{Deserialize, Serialize};

use crate::config::CopilotAgentTaskReactionConfig;

/// One row of the launch query's result set.
///
/// The `runId` binds the Project Item, the subject issue, and the exact issue
/// body digest. The reaction re-derives it from live GitHub state and requires
/// an exact match, so a body edited since the row was emitted aborts the launch
/// with zero side effects rather than proceeding on stale information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LaunchRow {
    /// `"owner/repo"`.
    pub repository: String,
    /// The subject issue number (`> 0`).
    pub subject_number: u64,
    /// The subject issue node ID (`I_...`).
    pub subject_node_id: String,
    /// The Project (v2) node ID (`PVT_...`).
    pub project_node_id: String,
    /// The Project (v2) item node ID (`PVTI_...`).
    pub project_item_node_id: String,
    /// The deterministic run identifier (`run:<64-hex>`).
    pub run_id: String,
    /// The model requested for the agent task.
    pub requested_model: String,
    /// An optional fallback model, tried once if the requested model is
    /// rejected as unsupported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    /// The git ref the agent task (and the pinned profile blob) is based on.
    pub base_ref: String,
}

impl LaunchRow {
    /// Parse a launch row from the raw `data` payload of an `Add` diff.
    pub fn from_json(data: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(data.clone()).context("launch row does not match expected schema")
    }

    /// Split `repository` into `(owner, repo)`.
    pub fn owner_and_repo(&self) -> Result<(&str, &str)> {
        let (owner, repo) = self
            .repository
            .split_once('/')
            .with_context(|| format!("repository '{}' is not 'owner/repo'", self.repository))?;
        if owner.is_empty() || repo.is_empty() || repo.contains('/') {
            bail!("repository '{}' is not 'owner/repo'", self.repository);
        }
        Ok((owner, repo))
    }

    /// Validate a parsed row against the reaction's configured allowlists and
    /// the frozen identifier grammar. Fails closed: an empty allowlist allows
    /// nothing. Every failure is **permanent** — retrying the identical row can
    /// never change the outcome, so the reaction logs and skips it.
    pub fn validate(&self, config: &CopilotAgentTaskReactionConfig) -> Result<()> {
        if !config
            .allowed_repositories
            .iter()
            .any(|allowed| allowed == &self.repository)
        {
            bail!(
                "repository '{}' is not in allowedRepositories",
                self.repository
            );
        }
        // Ensure the allowlisted value is also a usable 'owner/repo'.
        self.owner_and_repo()?;

        if self.subject_number == 0 {
            bail!("subjectNumber must be greater than 0");
        }
        if !self.subject_node_id.starts_with("I_") {
            bail!(
                "subjectNodeId '{}' must be a GitHub issue node ID starting with 'I_'",
                self.subject_node_id
            );
        }
        if !self.project_node_id.starts_with("PVT_") {
            bail!(
                "projectNodeId '{}' must be a GitHub Projects v2 node ID starting with 'PVT_'",
                self.project_node_id
            );
        }
        if !self.project_item_node_id.starts_with("PVTI_") {
            bail!(
                "projectItemNodeId '{}' must be a GitHub Projects v2 item node ID starting with 'PVTI_'",
                self.project_item_node_id
            );
        }
        RunId::try_from(self.run_id.clone())
            .map_err(|error| anyhow::anyhow!("runId '{}' is invalid: {error}", self.run_id))?;

        if !config
            .allowed_models
            .iter()
            .any(|allowed| allowed == &self.requested_model)
        {
            bail!(
                "requestedModel '{}' is not in allowedModels",
                self.requested_model
            );
        }
        if let Some(fallback) = &self.fallback_model {
            if fallback.is_empty() {
                bail!("fallbackModel must not be empty when present");
            }
            if !config
                .allowed_models
                .iter()
                .any(|allowed| allowed == fallback)
            {
                bail!("fallbackModel '{fallback}' is not in allowedModels");
            }
            if fallback == &self.requested_model {
                bail!("fallbackModel must differ from requestedModel");
            }
        }
        if self.base_ref.trim().is_empty() {
            bail!("baseRef must not be empty");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RUN_ID: &str = "run:1111111111111111111111111111111111111111111111111111111111111111";

    fn config() -> CopilotAgentTaskReactionConfig {
        CopilotAgentTaskReactionConfig {
            allowed_repositories: vec!["drasi-project/drasi-core".to_string()],
            allowed_profiles: vec!["issue-validator".to_string()],
            allowed_models: vec!["gpt-5".to_string(), "gpt-4".to_string()],
            ..Default::default()
        }
    }

    fn sample_row() -> LaunchRow {
        LaunchRow {
            repository: "drasi-project/drasi-core".to_string(),
            subject_number: 42,
            subject_node_id: "I_kwDOtest".to_string(),
            project_node_id: "PVT_test".to_string(),
            project_item_node_id: "PVTI_test".to_string(),
            run_id: RUN_ID.to_string(),
            requested_model: "gpt-5".to_string(),
            fallback_model: Some("gpt-4".to_string()),
            base_ref: "main".to_string(),
        }
    }

    #[test]
    fn parses_from_json_and_round_trips() {
        let row = sample_row();
        let value = serde_json::to_value(&row).expect("serialize");
        assert_eq!(LaunchRow::from_json(&value).expect("parse"), row);
    }

    #[test]
    fn rejects_unknown_fields() {
        let mut value = serde_json::to_value(sample_row()).expect("serialize");
        value["surpriseField"] = json!("nope");
        assert!(LaunchRow::from_json(&value).is_err());
    }

    #[test]
    fn rejects_missing_fields() {
        let value = json!({ "repository": "only-one-field" });
        assert!(LaunchRow::from_json(&value).is_err());
    }

    #[test]
    fn owner_and_repo_splits() {
        let row = sample_row();
        let (owner, repo) = row.owner_and_repo().expect("split");
        assert_eq!(owner, "drasi-project");
        assert_eq!(repo, "drasi-core");
    }

    #[test]
    fn valid_row_passes() {
        sample_row().validate(&config()).expect("valid row");
    }

    #[test]
    fn rejects_disallowed_repository() {
        let mut row = sample_row();
        row.repository = "evil/repo".to_string();
        assert!(row
            .validate(&config())
            .expect_err("disallowed repo")
            .to_string()
            .contains("allowedRepositories"));
    }

    #[test]
    fn fails_closed_on_empty_allowlists() {
        let error = sample_row()
            .validate(&CopilotAgentTaskReactionConfig::default())
            .expect_err("empty allowlists allow nothing");
        assert!(error.to_string().contains("allowedRepositories"));
    }

    #[test]
    fn rejects_disallowed_requested_model() {
        let mut row = sample_row();
        row.requested_model = "gpt-does-not-exist".to_string();
        assert!(row
            .validate(&config())
            .expect_err("disallowed model")
            .to_string()
            .contains("requestedModel"));
    }

    #[test]
    fn rejects_disallowed_fallback_model() {
        let mut row = sample_row();
        row.fallback_model = Some("gpt-nope".to_string());
        assert!(row
            .validate(&config())
            .expect_err("disallowed fallback")
            .to_string()
            .contains("fallbackModel"));
    }

    #[test]
    fn rejects_fallback_equal_to_requested() {
        let mut row = sample_row();
        row.fallback_model = Some(row.requested_model.clone());
        assert!(row
            .validate(&config())
            .expect_err("fallback equals requested")
            .to_string()
            .contains("differ"));
    }

    #[test]
    fn allows_missing_fallback_model() {
        let mut row = sample_row();
        row.fallback_model = None;
        row.validate(&config()).expect("missing fallback is fine");
    }

    #[test]
    fn rejects_bad_node_id_prefixes() {
        for mutate in [
            (|r: &mut LaunchRow| r.subject_node_id = "X_bad".to_string()) as fn(&mut LaunchRow),
            |r: &mut LaunchRow| r.project_node_id = "PVTI_wrong".to_string(),
            |r: &mut LaunchRow| r.project_item_node_id = "PVT_wrong".to_string(),
        ] {
            let mut row = sample_row();
            mutate(&mut row);
            assert!(row.validate(&config()).is_err());
        }
    }

    #[test]
    fn rejects_malformed_run_id() {
        let mut row = sample_row();
        row.run_id = "run:not-hex".to_string();
        assert!(row
            .validate(&config())
            .expect_err("bad run id")
            .to_string()
            .contains("runId"));
    }

    #[test]
    fn rejects_zero_subject_number() {
        let mut row = sample_row();
        row.subject_number = 0;
        assert!(row.validate(&config()).is_err());
    }
}
