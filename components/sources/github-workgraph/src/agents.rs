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
//! This module owns parsing and validation exactly once. Source startup and
//! relevant live `push` deliveries both call [`parse_agent_file`].
//!
//! Every rejection is an explicit [`WorkGraphError`]. A malformed or missing
//! required agent file must never degrade into a silently empty agent pool.

use crate::model::{slot_id, WorkGraphError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The original agent file version, carrying `agents` entries only.
pub const SUPPORTED_AGENT_FILE_VERSION: u64 = 1;
/// The actor catalog version, which adds `actors` entries alongside `agents`.
pub const SUPPORTED_ACTOR_FILE_VERSION: u64 = 2;
/// Every agent file version this Source understands.
pub const SUPPORTED_AGENT_FILE_VERSIONS: [u64; 2] =
    [SUPPORTED_AGENT_FILE_VERSION, SUPPORTED_ACTOR_FILE_VERSION];
/// Upper bound on configured agents in one file.
pub const MAX_AGENTS: usize = 64;
/// Upper bound on the concurrent slots one agent may declare.
///
/// The bound is part of the contract: `slots` is a *positive bounded* integer,
/// and the bound keeps the derived slot node count per agent predictable.
pub const MAX_AGENT_SLOTS: u32 = 16;
/// Upper bound on an agent ID, so derived slot IDs stay bounded too.
pub const MAX_AGENT_ID_LEN: usize = 64;
/// Upper bound on a GitHub login recorded for a human actor.
pub const MAX_ACTOR_LOGIN_LEN: usize = 39;
/// Upper bound on a GitHub node ID recorded for a human actor.
pub const MAX_ACTOR_NODE_ID_LEN: usize = 128;
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

/// What kind of executor one catalog entry describes.
///
/// A `version: 1` file describes agents only, so every legacy entry is an
/// [`ActorKind::Agent`]. `version: 2` adds humans, which are GitHub accounts
/// rather than orchestrated agents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActorKind {
    #[default]
    Agent,
    Human,
}

impl ActorKind {
    /// The exact wire spelling this kind is written and projected with.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Human => "human",
        }
    }
}

/// The exact GitHub account a human actor speaks as.
///
/// All three fields are required. The numeric `database_id` is the stable
/// identity: a GitHub account keeps it across renames and across the legacy
/// and next-generation node ID encodings. `node_id` and `login` are recorded
/// so ingress can corroborate a webhook payload without a second read.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActorGitHubIdentity {
    pub database_id: u64,
    pub node_id: String,
    pub login: String,
}

/// One validated catalog entry, whatever version declared it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActorDefinition {
    pub actor_id: String,
    pub kind: ActorKind,
    pub slots: u32,
    /// The exact `leaseDuration` text as written in the file.
    pub lease_duration: String,
    /// The same duration in whole seconds, so a Reaction can compute an
    /// `expiresAt` without re-parsing ISO-8601.
    pub lease_duration_seconds: i64,
    /// The custom agent an agent actor runs as. Defaults to `actor_id`, which
    /// is exactly what every `version: 1` agent means.
    pub custom_agent: String,
    /// Present for, and only for, a human actor.
    pub github: Option<ActorGitHubIdentity>,
}

impl ActorDefinition {
    /// The deterministic one-based slot IDs of this actor.
    pub fn slot_ids(&self) -> Vec<String> {
        (1..=self.slots)
            .map(|number| slot_id(&self.actor_id, number))
            .collect()
    }

    /// Whether this actor is the GitHub account the payload identifies.
    ///
    /// The numeric ID is the identity: it is issued by GitHub, survives a
    /// rename, and is the same across the legacy and next-generation node ID
    /// encodings. A login or node ID that has drifted since the catalog was
    /// written must not revoke a human's authority, so those are recorded for
    /// operators rather than compared. A zero ID is malformed and matches
    /// nothing.
    pub fn matches_github_identity(&self, database_id: u64) -> bool {
        database_id > 0
            && self
                .github
                .as_ref()
                .is_some_and(|identity| identity.database_id == database_id)
    }

    fn as_agent(&self) -> AgentDefinition {
        AgentDefinition {
            agent_id: self.actor_id.clone(),
            slots: self.slots,
            lease_duration: self.lease_duration.clone(),
            lease_duration_seconds: self.lease_duration_seconds,
        }
    }
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
///
/// `agents` stays exactly what every existing caller expects: the flat lease
/// capacity of every configured executor, agent and human alike. `actors`
/// carries the kind and GitHub identity the same entries were declared with,
/// so mapping and ingress can tell them apart without a second parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentFile {
    pub version: u64,
    pub agents: Vec<AgentDefinition>,
    pub actors: Vec<ActorDefinition>,
}

impl AgentFile {
    /// Build a file from agent definitions alone, exactly as `version: 1`
    /// declares them: every entry is an agent that runs as itself.
    pub fn from_agents(version: u64, agents: Vec<AgentDefinition>) -> Self {
        let actors = agents
            .iter()
            .map(|agent| ActorDefinition {
                actor_id: agent.agent_id.clone(),
                kind: ActorKind::Agent,
                slots: agent.slots,
                lease_duration: agent.lease_duration.clone(),
                lease_duration_seconds: agent.lease_duration_seconds,
                custom_agent: agent.agent_id.clone(),
                github: None,
            })
            .collect();
        Self {
            version,
            agents,
            actors,
        }
    }

    /// The catalog entry one executor ID names.
    pub fn actor(&self, actor_id: &str) -> Option<&ActorDefinition> {
        self.actors.iter().find(|actor| actor.actor_id == actor_id)
    }

    /// Every declared human actor, in catalog order.
    pub fn humans(&self) -> impl Iterator<Item = &ActorDefinition> {
        self.actors
            .iter()
            .filter(|actor| actor.kind == ActorKind::Human)
    }

    /// The human actor one GitHub account maps to, if the catalog declares it.
    pub fn human_for_github_identity(&self, database_id: u64) -> Option<&ActorDefinition> {
        self.humans()
            .find(|actor| actor.matches_github_identity(database_id))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentFileRoot {
    version: u64,
    #[serde(default)]
    agents: Vec<AgentRoot>,
    #[serde(default)]
    actors: Vec<ActorRoot>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentRoot {
    agent_id: String,
    slots: u32,
    lease_duration: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActorRoot {
    actor_id: String,
    kind: String,
    slots: u32,
    lease_duration: String,
    #[serde(default)]
    custom_agent: Option<String>,
    #[serde(default)]
    github: Option<ActorGitHubRoot>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActorGitHubRoot {
    database_id: u64,
    node_id: String,
    login: String,
}

/// Parse and strictly validate an agent file.
///
/// Rejects anything that is not LF UTF-8 text of at most 256 KiB. `version: 1`
/// carries `agents` only and parses exactly as it always has. `version: 2`
/// adds `actors`, which declare a kind and, for a human, the exact GitHub
/// account that human speaks as.
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
    if !SUPPORTED_AGENT_FILE_VERSIONS.contains(&root.version) {
        return Err(format!(
            "version must equal {SUPPORTED_AGENT_FILE_VERSION} or \
             {SUPPORTED_ACTOR_FILE_VERSION}, found {}",
            root.version
        ));
    }
    // Each version declares its executors exactly one way: `version: 1` lists
    // `agents`, `version: 2` lists `actors`. Mixing them would give one file
    // two identity namespaces with no defined precedence.
    if root.version == SUPPORTED_AGENT_FILE_VERSION && !root.actors.is_empty() {
        return Err(format!(
            "actors requires version {SUPPORTED_ACTOR_FILE_VERSION}"
        ));
    }
    if root.version == SUPPORTED_ACTOR_FILE_VERSION && !root.agents.is_empty() {
        return Err(format!(
            "version {SUPPORTED_ACTOR_FILE_VERSION} declares executors under 'actors'; \
             'agents' requires version {SUPPORTED_AGENT_FILE_VERSION}"
        ));
    }
    if root.agents.is_empty() && root.actors.is_empty() {
        return Err(match root.version {
            SUPPORTED_ACTOR_FILE_VERSION => "actors must contain at least one actor".to_string(),
            _ => "agents must contain at least one agent".to_string(),
        });
    }
    if root.agents.len() + root.actors.len() > MAX_AGENTS {
        return Err(format!(
            "agents must contain at most {MAX_AGENTS} agents, found {}",
            root.agents.len() + root.actors.len()
        ));
    }

    let mut actors = Vec::with_capacity(root.agents.len() + root.actors.len());
    for (index, agent) in root.agents.into_iter().enumerate() {
        actors.push(parse_agent(index, agent)?);
    }
    let declared_agents = actors.len();
    for (index, actor) in root.actors.into_iter().enumerate() {
        actors.push(parse_actor(index, actor)?);
    }

    // Identity and slot namespaces are shared, so no two entries can shadow
    // each other or double-book the same capacity.
    let mut seen_actor_ids = BTreeSet::new();
    let mut seen_slot_ids = BTreeSet::new();
    let mut seen_github_identities = BTreeSet::new();
    for (index, actor) in actors.iter().enumerate() {
        let field = |name: &str| {
            if index < declared_agents {
                format!("agents[{index}].{name}")
            } else {
                format!("actors[{}].{name}", index - declared_agents)
            }
        };
        if !seen_actor_ids.insert(actor.actor_id.clone()) {
            return Err(format!(
                "{} '{}' is duplicated; actor IDs must be unique",
                field(if index < declared_agents {
                    "agentId"
                } else {
                    "actorId"
                }),
                actor.actor_id
            ));
        }
        for slot in actor.slot_ids() {
            if !seen_slot_ids.insert(slot.clone()) {
                return Err(format!(
                    "{} derives slot ID '{slot}', which is already claimed by another actor",
                    field("slots")
                ));
            }
        }
        if let Some(github) = &actor.github {
            if !seen_github_identities.insert(github.database_id) {
                return Err(format!(
                    "{} databaseId {} is already claimed by another human actor",
                    field("github"),
                    github.database_id
                ));
            }
        }
    }

    Ok(AgentFile {
        version: root.version,
        agents: actors.iter().map(ActorDefinition::as_agent).collect(),
        actors,
    })
}

fn parse_agent(index: usize, agent: AgentRoot) -> Result<ActorDefinition, String> {
    let field = |name: &str| format!("agents[{index}].{name}");
    validate_agent_id(&agent.agent_id, &field("agentId"))?;
    let lease_duration_seconds =
        parse_lease_duration(&agent.lease_duration, &field("leaseDuration"))?;
    validate_slots(agent.slots, &field("slots"))?;
    Ok(ActorDefinition {
        custom_agent: agent.agent_id.clone(),
        actor_id: agent.agent_id,
        kind: ActorKind::Agent,
        slots: agent.slots,
        lease_duration: agent.lease_duration,
        lease_duration_seconds,
        github: None,
    })
}

fn parse_actor(index: usize, actor: ActorRoot) -> Result<ActorDefinition, String> {
    let field = |name: &str| format!("actors[{index}].{name}");
    validate_agent_id(&actor.actor_id, &field("actorId"))?;
    let kind = match actor.kind.as_str() {
        "agent" => ActorKind::Agent,
        "human" => ActorKind::Human,
        other => {
            return Err(format!(
                "{} must be 'agent' or 'human', found '{other}'",
                field("kind")
            ))
        }
    };
    let lease_duration_seconds =
        parse_lease_duration(&actor.lease_duration, &field("leaseDuration"))?;
    validate_slots(actor.slots, &field("slots"))?;
    let custom_agent = match (kind, actor.custom_agent) {
        (ActorKind::Agent, Some(custom_agent)) => {
            validate_agent_id(&custom_agent, &field("customAgent"))?;
            custom_agent
        }
        // An agent actor that names no custom agent runs as itself, which is
        // exactly what a `version: 1` agent has always meant.
        (ActorKind::Agent, None) => actor.actor_id.clone(),
        (ActorKind::Human, Some(_)) => {
            return Err(format!(
                "{} is only valid for an agent actor",
                field("customAgent")
            ))
        }
        (ActorKind::Human, None) => actor.actor_id.clone(),
    };
    let github = match (kind, actor.github) {
        (ActorKind::Human, Some(github)) => Some(parse_github_identity(github, &field("github"))?),
        (ActorKind::Human, None) => {
            return Err(format!("{} is required for a human actor", field("github")))
        }
        (ActorKind::Agent, Some(_)) => {
            return Err(format!(
                "{} is only valid for a human actor",
                field("github")
            ))
        }
        (ActorKind::Agent, None) => None,
    };
    Ok(ActorDefinition {
        actor_id: actor.actor_id,
        kind,
        slots: actor.slots,
        lease_duration: actor.lease_duration,
        lease_duration_seconds,
        custom_agent,
        github,
    })
}

fn parse_github_identity(
    github: ActorGitHubRoot,
    field: &str,
) -> Result<ActorGitHubIdentity, String> {
    if github.database_id == 0 {
        return Err(format!(
            "{field}.databaseId must be a positive GitHub user ID"
        ));
    }
    validate_opaque_actor_field(
        &github.node_id,
        MAX_ACTOR_NODE_ID_LEN,
        &format!("{field}.nodeId"),
    )?;
    validate_opaque_actor_field(
        &github.login,
        MAX_ACTOR_LOGIN_LEN,
        &format!("{field}.login"),
    )?;
    Ok(ActorGitHubIdentity {
        database_id: github.database_id,
        node_id: github.node_id,
        login: github.login,
    })
}

fn validate_opaque_actor_field(value: &str, max_len: usize, field: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > max_len {
        return Err(format!("{field} must be 1 to {max_len} characters"));
    }
    if value.chars().any(char::is_whitespace) || value.chars().any(char::is_control) {
        return Err(format!(
            "{field} must not contain whitespace or control characters"
        ));
    }
    Ok(())
}

fn validate_slots(slots: u32, field: &str) -> Result<(), String> {
    if slots == 0 || slots > MAX_AGENT_SLOTS {
        return Err(format!(
            "{field} must be between 1 and {MAX_AGENT_SLOTS}, found {slots}"
        ));
    }
    Ok(())
}

fn parse_lease_duration(lease_duration: &str, field: &str) -> Result<i64, String> {
    let lease_duration_seconds =
        parse_iso8601_duration_seconds(lease_duration).ok_or_else(|| {
            format!(
            "{field} must be a positive ISO-8601 duration built from whole days, hours, minutes, \
             and seconds, for example 'PT15M'"
        )
        })?;
    if !(MIN_LEASE_DURATION_SECONDS..=MAX_LEASE_DURATION_SECONDS).contains(&lease_duration_seconds)
    {
        return Err(format!(
            "{field} must be between {MIN_LEASE_DURATION_SECONDS}s and \
             {MAX_LEASE_DURATION_SECONDS}s, found {lease_duration_seconds}s"
        ));
    }
    Ok(lease_duration_seconds)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The one deterministic human the WorkGraph mock simulates.
    const HUMAN_ACTOR_ID: &str = "human-agentofreality";
    const HUMAN_DATABASE_ID: u64 = 4_021_243;
    const HUMAN_NODE_ID: &str = "MDQ6VXNlcjQwMjEyNDM=";
    const HUMAN_LOGIN: &str = "agentofreality";

    const V1_FILE: &str = "version: 1\nagents:\n- agentId: executor\n  slots: 2\n  \
                           leaseDuration: PT15M\n";

    #[test]
    fn version_one_agents_parse_exactly_as_they_always_have() {
        let file = parse_agent_file(V1_FILE).expect("version 1 agent file");
        assert_eq!(file.version, 1);
        assert_eq!(
            file.agents,
            vec![AgentDefinition {
                agent_id: "executor".to_string(),
                slots: 2,
                lease_duration: "PT15M".to_string(),
                lease_duration_seconds: 900,
            }]
        );
        // The catalog view of a legacy file is one agent that runs as itself
        // and carries no GitHub identity.
        assert_eq!(file.actors.len(), 1);
        let actor = &file.actors[0];
        assert_eq!(actor.actor_id, "executor");
        assert_eq!(actor.kind, ActorKind::Agent);
        assert_eq!(actor.custom_agent, "executor");
        assert!(actor.github.is_none());
        assert_eq!(actor.slot_ids(), vec!["executor/1", "executor/2"]);
        assert!(file.humans().next().is_none());
    }

    #[test]
    fn a_version_two_agent_actor_matches_the_version_one_result() {
        let v2 = parse_agent_file(
            "version: 2\nactors:\n- actorId: executor\n  kind: agent\n  slots: 2\n  \
             leaseDuration: PT15M\n",
        )
        .expect("version 2 actor file");
        let v1 = parse_agent_file(V1_FILE).expect("version 1 agent file");
        assert_eq!(v2.agents, v1.agents);
        assert_eq!(v2.actors, v1.actors);
        assert_eq!(v2.version, 2);
    }

    #[test]
    fn each_version_declares_its_executors_exactly_one_way() {
        // A version 1 file may only list agents.
        let error = parse_agent_file(
            "version: 1\nagents:\n- agentId: executor\n  slots: 1\n  leaseDuration: PT15M\n\
             actors:\n- actorId: human-agentofreality\n  kind: human\n  slots: 1\n  \
             leaseDuration: PT8H\n  github:\n    databaseId: 4021243\n    \
             nodeId: MDQ6VXNlcjQwMjEyNDM=\n    login: agentofreality\n",
        )
        .expect_err("version 1 must reject actors");
        assert_eq!(error.code, error_code::INVALID_AGENT_FILE_PAYLOAD);
        assert!(error.message.contains("actors requires version 2"));

        // A version 2 file may only list actors, so one file never carries
        // two executor namespaces.
        let error = parse_agent_file(
            "version: 2\nagents:\n- agentId: executor\n  slots: 1\n  leaseDuration: PT15M\n\
             actors:\n- actorId: worker\n  kind: agent\n  slots: 1\n  leaseDuration: PT15M\n",
        )
        .expect_err("version 2 must reject agents");
        assert!(error
            .message
            .contains("version 2 declares executors under 'actors'"));
        let error = parse_agent_file(
            "version: 2\nagents:\n- agentId: executor\n  slots: 1\n  leaseDuration: PT15M\n",
        )
        .expect_err("version 2 must reject an agents-only file");
        assert!(error
            .message
            .contains("version 2 declares executors under 'actors'"));

        let error = parse_agent_file(
            "version: 3\nagents:\n- agentId: executor\n  slots: 1\n  leaseDuration: PT15M\n",
        )
        .expect_err("version 3 is unknown");
        assert!(error.message.contains("version must equal 1 or 2"));
    }

    #[test]
    fn the_exact_agentofreality_human_actor_parses() {
        let file = parse_agent_file(
            "version: 2\nactors:\n- actorId: executor\n  kind: agent\n  slots: 1\n  \
             leaseDuration: PT15M\n- actorId: human-agentofreality\n  kind: human\n  \
             slots: 1\n  leaseDuration: PT8H\n  github:\n    databaseId: 4021243\n    \
             nodeId: MDQ6VXNlcjQwMjEyNDM=\n    login: agentofreality\n",
        )
        .expect("version 2 actor catalog");
        assert_eq!(file.version, 2);
        // A human is lease capacity exactly like an agent, so it appears in
        // the flat agent view every allocator path already reads.
        assert_eq!(file.agents.len(), 2);
        assert!(file
            .agents
            .iter()
            .any(|agent| agent.agent_id == HUMAN_ACTOR_ID && agent.slots == 1));
        let human = file.actor(HUMAN_ACTOR_ID).expect("human actor");
        assert_eq!(human.kind, ActorKind::Human);
        assert_eq!(human.lease_duration_seconds, 8 * 3_600);
        let github = human.github.as_ref().expect("human GitHub identity");
        assert_eq!(github.database_id, HUMAN_DATABASE_ID);
        assert_eq!(github.node_id, HUMAN_NODE_ID);
        assert_eq!(github.login, HUMAN_LOGIN);
        assert_eq!(human.slot_ids(), vec!["human-agentofreality/1"]);
        assert_eq!(
            file.human_for_github_identity(HUMAN_DATABASE_ID)
                .map(|actor| actor.actor_id.as_str()),
            Some(HUMAN_ACTOR_ID)
        );
        // The numeric ID is the identity: a rename or a re-encoded node ID
        // must not revoke a human, and a different or malformed ID matches
        // nothing.
        assert!(file.human_for_github_identity(1).is_none());
        assert!(file.human_for_github_identity(0).is_none());
        assert!(!human.matches_github_identity(0));
        assert!(human.matches_github_identity(HUMAN_DATABASE_ID));
    }

    #[test]
    fn an_agent_actor_defaults_its_custom_agent_to_its_actor_id() {
        let file = parse_agent_file(
            "version: 2\nactors:\n- actorId: worker\n  kind: agent\n  slots: 1\n  \
             leaseDuration: PT15M\n- actorId: reviewer\n  kind: agent\n  slots: 1\n  \
             leaseDuration: PT15M\n  customAgent: shared-reviewer\n",
        )
        .expect("version 2 agent actors");
        assert_eq!(file.actor("worker").unwrap().custom_agent, "worker");
        assert_eq!(
            file.actor("reviewer").unwrap().custom_agent,
            "shared-reviewer"
        );
    }

    #[test]
    fn actor_declarations_are_strictly_validated() {
        let cases = [
            (
                "version: 2\nactors:\n- actorId: someone\n  kind: human\n  slots: 1\n  \
                 leaseDuration: PT8H\n",
                "github is required for a human actor",
            ),
            (
                "version: 2\nactors:\n- actorId: worker\n  kind: agent\n  slots: 1\n  \
                 leaseDuration: PT15M\n  github:\n    databaseId: 1\n    nodeId: n\n    \
                 login: l\n",
                "only valid for a human actor",
            ),
            (
                "version: 2\nactors:\n- actorId: someone\n  kind: human\n  slots: 1\n  \
                 leaseDuration: PT8H\n  customAgent: x\n  github:\n    databaseId: 1\n    \
                 nodeId: n\n    login: l\n",
                "only valid for an agent actor",
            ),
            (
                "version: 2\nactors:\n- actorId: worker\n  kind: robot\n  slots: 1\n  \
                 leaseDuration: PT15M\n",
                "must be 'agent' or 'human'",
            ),
            (
                "version: 2\nactors:\n- actorId: worker\n  kind: agent\n  slots: 0\n  \
                 leaseDuration: PT15M\n",
                "must be between 1 and 16",
            ),
            (
                "version: 2\nactors:\n- actorId: worker\n  kind: agent\n  slots: 1\n  \
                 leaseDuration: PT0S\n",
                "must be between 1s and 86400s",
            ),
            (
                "version: 2\nactors:\n- actorId: someone\n  kind: human\n  slots: 1\n  \
                 leaseDuration: PT8H\n  github:\n    databaseId: 0\n    nodeId: n\n    \
                 login: l\n",
                "must be a positive GitHub user ID",
            ),
            (
                "version: 2\nactors:\n- actorId: someone\n  kind: human\n  slots: 1\n  \
                 leaseDuration: PT8H\n  github:\n    databaseId: 1\n    nodeId: \"\"\n    \
                 login: l\n",
                "must be 1 to 128 characters",
            ),
            (
                "version: 2\nactors:\n- actorId: worker\n  kind: agent\n  slots: 1\n  \
                 leaseDuration: PT15M\n- actorId: worker\n  kind: agent\n  slots: 1\n  \
                 leaseDuration: PT15M\n",
                "actor IDs must be unique",
            ),
            (
                "version: 2\nactors:\n- actorId: a\n  kind: human\n  slots: 1\n  \
                 leaseDuration: PT8H\n  github:\n    databaseId: 7\n    nodeId: n\n    \
                 login: l\n- actorId: b\n  kind: human\n  slots: 1\n  leaseDuration: PT8H\n  \
                 github:\n    databaseId: 7\n    nodeId: m\n    login: k\n",
                "already claimed by another human actor",
            ),
            (
                "version: 2\nactors:\n- actorId: worker\n  kind: agent\n  slots: 1\n  \
                 leaseDuration: PT15M\n  unexpected: true\n",
                "unknown field",
            ),
        ];
        for (text, expected) in cases {
            let error = parse_agent_file(text).expect_err(expected);
            assert!(
                error.message.contains(expected),
                "'{}' does not contain '{expected}'",
                error.message
            );
        }
    }

    #[test]
    fn an_empty_catalog_and_a_bounded_one_are_both_refused() {
        assert!(parse_agent_file("version: 2\n")
            .expect_err("empty catalog")
            .message
            .contains("actors must contain at least one actor"));
        let mut text = String::from("version: 2\nactors:\n");
        for index in 0..=MAX_AGENTS {
            text.push_str(&format!(
                "- actorId: worker-{index}\n  kind: agent\n  slots: 1\n  leaseDuration: PT15M\n"
            ));
        }
        assert!(parse_agent_file(&text)
            .expect_err("bounded catalog")
            .message
            .contains("at most 64 agents"));
    }
}
