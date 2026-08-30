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

//! The strict `version: 1` agent-capacity configuration file contract.
//!
//! The configured set of agents and their capacity lives in a versioned
//! repository file (normally `.github/workgraph/agents.yaml`). It describes
//! *desired* agent capacity only; Assignments are GitHub comments and active
//! Leases are synthetic Source state.
//!
//! This module owns parsing and validation exactly once. Both the streaming
//! Source (on a relevant `push`) and the bootstrapper (before it projects any
//! task artifact) call [`parse_agent_file`], so a file that one accepts the
//! other accepts identically.
//!
//! Every rejection is an explicit [`WorkGraphError`]. A malformed or missing
//! required agent file must never degrade into a silently empty agent pool.

use crate::model::{slot_id, WorkGraphError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Only `version: 1` agent files are understood.
pub const SUPPORTED_AGENT_FILE_VERSION: u64 = 1;
/// Upper bound on configured agents in one file.
pub const MAX_AGENTS: usize = 64;
/// Upper bound on the concurrent slots one agent may declare.
///
/// The bound is part of the contract: `slots` is a *positive bounded* integer,
/// and the bound keeps the derived slot node count per agent predictable.
pub const MAX_AGENT_SLOTS: u32 = 16;
/// Upper bound on an agent ID, so derived slot IDs stay bounded too.
pub const MAX_AGENT_ID_LEN: usize = 64;
/// Lower bound on a lease duration; a non-positive lease can never be held.
pub const MIN_LEASE_DURATION_SECONDS: i64 = 1;
/// Upper bound on a lease duration. A lease longer than a day would keep a
/// slot unusable far past any plausible agent execution.
pub const MAX_LEASE_DURATION_SECONDS: i64 = 24 * 60 * 60;
/// Upper bound on the raw agent file the Source will parse.
pub const MAX_AGENT_FILE_BYTES: u64 = 256 * 1024;

pub mod error_code {
    pub const AGENT_FILE_UNAVAILABLE: &str = "agent-file-unavailable";
    pub const AGENT_FILE_TOO_LARGE: &str = "agent-file-too-large";
    pub const INVALID_AGENT_FILE_YAML: &str = "invalid-agent-file-yaml";
    pub const INVALID_AGENT_FILE_PAYLOAD: &str = "invalid-agent-file-payload";
}

/// The exact repository location of the agent file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentFileLocation {
    /// `owner/name` of the repository holding the agent file.
    pub repository: String,
    /// The exact git ref (normally a branch name such as `main`).
    pub r#ref: String,
    /// The exact repository-relative path of the agent file.
    pub path: String,
}

impl AgentFileLocation {
    pub fn validate(&self) -> anyhow::Result<()> {
        let (owner, name) = self
            .repository
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("agentConfig.repository must be 'owner/name'"))?;
        anyhow::ensure!(
            !owner.is_empty()
                && !name.is_empty()
                && !name.contains('/')
                && self.repository.trim() == self.repository,
            "agentConfig.repository must be exactly one 'owner/name' pair without surrounding whitespace"
        );
        anyhow::ensure!(
            !self.r#ref.trim().is_empty() && self.r#ref.trim() == self.r#ref,
            "agentConfig.ref must be a non-empty git ref without surrounding whitespace"
        );
        anyhow::ensure!(
            !self.r#ref.chars().any(char::is_whitespace) && !self.r#ref.contains(':'),
            "agentConfig.ref must not contain whitespace or ':'"
        );
        let path = &self.path;
        anyhow::ensure!(
            !path.trim().is_empty() && path.trim() == *path,
            "agentConfig.path must be a non-empty path without surrounding whitespace"
        );
        anyhow::ensure!(
            !path.starts_with('/')
                && !path.contains("//")
                && !path
                    .split('/')
                    .any(|segment| segment == ".." || segment == ".")
                && !path.chars().any(char::is_whitespace),
            "agentConfig.path must be a normalized repository-relative path without '.', '..', \
             empty segments, or whitespace"
        );
        Ok(())
    }

    /// The `owner` half of `repository`.
    pub fn owner(&self) -> &str {
        self.repository
            .split_once('/')
            .map_or("", |(owner, _)| owner)
    }

    /// The `name` half of `repository`.
    pub fn name(&self) -> &str {
        self.repository.split_once('/').map_or("", |(_, name)| name)
    }

    /// The GraphQL `object(expression:)` form addressing this exact blob.
    pub fn expression(&self) -> String {
        format!("{}:{}", self.r#ref, self.path)
    }

    /// True when a `push` delivery names exactly this repository and ref.
    pub fn matches_push(&self, repository_name_with_owner: &str, pushed_ref: &str) -> bool {
        repository_name_with_owner.eq_ignore_ascii_case(&self.repository)
            && (pushed_ref == self.r#ref
                || pushed_ref == format!("refs/heads/{}", self.r#ref)
                || pushed_ref == format!("refs/tags/{}", self.r#ref))
    }
}

/// The exact bytes fetched for an agent file, plus their content provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentFileContent {
    pub text: String,
    /// The git object ID of the blob, recorded as configuration provenance.
    pub oid: String,
}

/// One validated configured agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDefinition {
    pub agent_id: String,
    pub slots: u32,
    /// The exact `leaseDuration` text as written in the file.
    pub lease_duration: String,
    /// The same duration in whole seconds, so a Reaction can compute an
    /// `expiresAt` without re-parsing ISO-8601.
    pub lease_duration_seconds: i64,
}

impl AgentDefinition {
    /// The deterministic one-based slot IDs of this agent.
    pub fn slot_ids(&self) -> Vec<String> {
        (1..=self.slots)
            .map(|number| slot_id(&self.agent_id, number))
            .collect()
    }
}

/// A validated agent file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentFile {
    pub version: u64,
    pub agents: Vec<AgentDefinition>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentFileRoot {
    version: u64,
    agents: Vec<AgentRoot>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentRoot {
    agent_id: String,
    slots: u32,
    lease_duration: String,
}

/// Parse and strictly validate an agent file.
///
/// Rejects anything that is not LF UTF-8 text of at most 256 KiB and exactly
/// `version: 1` with a non-empty `agents` list of uniquely-identified agents.
pub fn parse_agent_file(text: &str) -> Result<AgentFile, WorkGraphError> {
    if text.len() as u64 > MAX_AGENT_FILE_BYTES {
        return Err(WorkGraphError::new(
            error_code::AGENT_FILE_TOO_LARGE,
            format!(
                "the agent file is {} bytes, exceeding the {MAX_AGENT_FILE_BYTES} byte limit",
                text.len()
            ),
        ));
    }
    if text.contains('\r') {
        return Err(WorkGraphError::new(
            error_code::INVALID_AGENT_FILE_PAYLOAD,
            "the agent file must use LF line endings",
        ));
    }
    let root: AgentFileRoot = serde_yaml::from_str(text).map_err(|error| {
        WorkGraphError::new(
            error_code::INVALID_AGENT_FILE_YAML,
            format!("invalid agent file YAML: {error}"),
        )
    })?;
    parse_root(root)
        .map_err(|message| WorkGraphError::new(error_code::INVALID_AGENT_FILE_PAYLOAD, message))
}

fn parse_root(root: AgentFileRoot) -> Result<AgentFile, String> {
    if root.version != SUPPORTED_AGENT_FILE_VERSION {
        return Err(format!(
            "version must equal {SUPPORTED_AGENT_FILE_VERSION}, found {}",
            root.version
        ));
    }
    if root.agents.is_empty() {
        return Err("agents must contain at least one agent".to_string());
    }
    if root.agents.len() > MAX_AGENTS {
        return Err(format!(
            "agents must contain at most {MAX_AGENTS} agents, found {}",
            root.agents.len()
        ));
    }

    let mut agents = Vec::with_capacity(root.agents.len());
    let mut seen_agent_ids = BTreeSet::new();
    let mut seen_slot_ids = BTreeSet::new();
    for (index, agent) in root.agents.into_iter().enumerate() {
        let agent = parse_agent(index, agent)?;
        if !seen_agent_ids.insert(agent.agent_id.clone()) {
            return Err(format!(
                "agents[{index}].agentId '{}' is duplicated; agent IDs must be unique",
                agent.agent_id
            ));
        }
        for slot in agent.slot_ids() {
            if !seen_slot_ids.insert(slot.clone()) {
                return Err(format!(
                    "agents[{index}] derives slot ID '{slot}', which is already claimed by \
                     another agent"
                ));
            }
        }
        agents.push(agent);
    }

    Ok(AgentFile {
        version: SUPPORTED_AGENT_FILE_VERSION,
        agents,
    })
}

fn parse_agent(index: usize, agent: AgentRoot) -> Result<AgentDefinition, String> {
    let field = |name: &str| format!("agents[{index}].{name}");
    validate_agent_id(&agent.agent_id, &field("agentId"))?;
    if agent.slots == 0 || agent.slots > MAX_AGENT_SLOTS {
        return Err(format!(
            "{} must be between 1 and {MAX_AGENT_SLOTS}, found {}",
            field("slots"),
            agent.slots
        ));
    }
    let lease_duration_seconds =
        parse_iso8601_duration_seconds(&agent.lease_duration).ok_or_else(|| {
            format!(
                "{} must be a positive ISO-8601 duration built from whole days, hours, minutes, \
                 and seconds, for example 'PT15M'",
                field("leaseDuration")
            )
        })?;
    if !(MIN_LEASE_DURATION_SECONDS..=MAX_LEASE_DURATION_SECONDS).contains(&lease_duration_seconds)
    {
        return Err(format!(
            "{} must be between {MIN_LEASE_DURATION_SECONDS}s and {MAX_LEASE_DURATION_SECONDS}s, \
             found {lease_duration_seconds}s",
            field("leaseDuration")
        ));
    }
    Ok(AgentDefinition {
        agent_id: agent.agent_id,
        slots: agent.slots,
        lease_duration: agent.lease_duration,
        lease_duration_seconds,
    })
}

/// Validate the case-sensitive agent ID used by Assignments and capacity files.
pub(crate) fn validate_agent_id(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_AGENT_ID_LEN {
        return Err(format!(
            "{field} must be 1 to {MAX_AGENT_ID_LEN} characters"
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte))
    {
        return Err(format!(
            "{field} must contain only ASCII letters, digits, '-', '.', or '_'"
        ));
    }
    Ok(())
}

/// Parse a strict `P[nD][T[nH][nM][nS]]` duration into whole seconds.
///
/// Only whole days, hours, minutes, and seconds are accepted. Calendar-relative
/// designators (`Y`, `M` before `T`, `W`) and fractional components are
/// rejected because a lease deadline must be an exact, offset-independent
/// number of seconds.
pub fn parse_iso8601_duration_seconds(text: &str) -> Option<i64> {
    let rest = text.strip_prefix('P')?;
    if rest.is_empty() {
        return None;
    }
    let (date_part, time_part) = match rest.split_once('T') {
        Some((date, time)) => {
            if time.is_empty() {
                return None;
            }
            (date, Some(time))
        }
        None => (rest, None),
    };

    let mut seconds: i64 = 0;
    let mut any = false;
    let mut consume = |part: &str, units: &[(char, i64)]| -> Option<()> {
        let mut digits = String::new();
        let mut next_unit = 0usize;
        for character in part.chars() {
            if character.is_ascii_digit() {
                digits.push(character);
                continue;
            }
            // Units must appear at most once and in descending order.
            let position = units[next_unit..]
                .iter()
                .position(|(unit, _)| *unit == character)?;
            let (_, multiplier) = units[next_unit + position];
            next_unit += position + 1;
            if digits.is_empty() {
                return None;
            }
            let value: i64 = digits.parse().ok()?;
            digits.clear();
            seconds = seconds.checked_add(value.checked_mul(multiplier)?)?;
            any = true;
        }
        // A trailing digit run without a designator is malformed.
        digits.is_empty().then_some(())
    };

    consume(date_part, &[('D', 86_400)])?;
    if let Some(time_part) = time_part {
        consume(time_part, &[('H', 3_600), ('M', 60), ('S', 1)])?;
    }
    any.then_some(seconds)
}
