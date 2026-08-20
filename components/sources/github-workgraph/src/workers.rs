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

//! The strict `version: 1` worker-queue configuration file contract.
//!
//! The configured set of workers and their capacity lives in a versioned
//! repository file (normally `.github/workgraph/workers.yaml`). It describes
//! *desired* worker capacity only — runtime claims stay in GitHub task
//! comments as Assignments and Leases.
//!
//! This module owns parsing and validation exactly once. Both the streaming
//! Source (on a relevant `push`) and the bootstrapper (before it projects any
//! task artifact) call [`parse_worker_file`], so a file that one accepts the
//! other accepts identically.
//!
//! Every rejection is an explicit [`WorkGraphError`]. A malformed or missing
//! required worker file must never degrade into a silently empty worker pool.

use crate::workgraph::{slot_id, WorkGraphError, SUPPORTED_AGENT_PROFILES};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Only `version: 1` worker files are understood.
pub const SUPPORTED_WORKER_FILE_VERSION: u64 = 1;
/// Upper bound on configured workers in one file.
pub const MAX_WORKERS: usize = 64;
/// Upper bound on the concurrent slots one worker may declare.
///
/// The bound is part of the contract: `slots` is a *positive bounded* integer,
/// and the bound keeps the derived slot node count per worker predictable.
pub const MAX_WORKER_SLOTS: u32 = 16;
/// Upper bound on a worker ID, so derived slot IDs stay bounded too.
pub const MAX_WORKER_ID_LEN: usize = 64;
/// Lower bound on a lease duration; a non-positive lease can never be held.
pub const MIN_LEASE_DURATION_SECONDS: i64 = 1;
/// Upper bound on a lease duration. A lease longer than a day would keep a
/// slot unusable far past any plausible worker execution.
pub const MAX_LEASE_DURATION_SECONDS: i64 = 24 * 60 * 60;
/// Upper bound on the raw worker file the Source will parse.
pub const MAX_WORKER_FILE_BYTES: u64 = 256 * 1024;

pub mod error_code {
    pub const WORKER_FILE_UNAVAILABLE: &str = "worker-file-unavailable";
    pub const WORKER_FILE_TOO_LARGE: &str = "worker-file-too-large";
    pub const INVALID_WORKER_FILE_YAML: &str = "invalid-worker-file-yaml";
    pub const INVALID_WORKER_FILE_PAYLOAD: &str = "invalid-worker-file-payload";
}

/// The exact repository location of the worker file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerFileLocation {
    /// `owner/name` of the repository holding the worker file.
    pub repository: String,
    /// The exact git ref (normally a branch name such as `main`).
    pub r#ref: String,
    /// The exact repository-relative path of the worker file.
    pub path: String,
}

impl WorkerFileLocation {
    pub fn validate(&self) -> anyhow::Result<()> {
        let (owner, name) = self
            .repository
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("workerConfig.repository must be 'owner/name'"))?;
        anyhow::ensure!(
            !owner.is_empty()
                && !name.is_empty()
                && !name.contains('/')
                && self.repository.trim() == self.repository,
            "workerConfig.repository must be exactly one 'owner/name' pair without surrounding whitespace"
        );
        anyhow::ensure!(
            !self.r#ref.trim().is_empty() && self.r#ref.trim() == self.r#ref,
            "workerConfig.ref must be a non-empty git ref without surrounding whitespace"
        );
        anyhow::ensure!(
            !self.r#ref.chars().any(char::is_whitespace) && !self.r#ref.contains(':'),
            "workerConfig.ref must not contain whitespace or ':'"
        );
        let path = &self.path;
        anyhow::ensure!(
            !path.trim().is_empty() && path.trim() == *path,
            "workerConfig.path must be a non-empty path without surrounding whitespace"
        );
        anyhow::ensure!(
            !path.starts_with('/')
                && !path.contains("//")
                && !path
                    .split('/')
                    .any(|segment| segment == ".." || segment == ".")
                && !path.chars().any(char::is_whitespace),
            "workerConfig.path must be a normalized repository-relative path without '.', '..', \
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

/// The exact bytes fetched for a worker file, plus their content provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerFileContent {
    pub text: String,
    /// The git object ID of the blob, recorded as configuration provenance.
    pub oid: String,
}

/// One validated configured worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerDefinition {
    pub worker_id: String,
    pub agent_profile: String,
    pub slots: u32,
    /// The exact `leaseDuration` text as written in the file.
    pub lease_duration: String,
    /// The same duration in whole seconds, so a Reaction can compute an
    /// `expiresAt` without re-parsing ISO-8601.
    pub lease_duration_seconds: i64,
}

impl WorkerDefinition {
    /// The deterministic one-based slot IDs of this worker.
    pub fn slot_ids(&self) -> Vec<String> {
        (1..=self.slots)
            .map(|number| slot_id(&self.worker_id, number))
            .collect()
    }
}

/// A validated worker file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerFile {
    pub version: u64,
    pub workers: Vec<WorkerDefinition>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkerFileRoot {
    version: u64,
    workers: Vec<WorkerRoot>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkerRoot {
    worker_id: String,
    agent_profile: String,
    slots: u32,
    lease_duration: String,
}

/// Parse and strictly validate a worker file.
///
/// Rejects anything that is not exactly `version: 1` with a non-empty `workers`
/// list of fully-specified, uniquely-identified workers.
pub fn parse_worker_file(text: &str) -> Result<WorkerFile, WorkGraphError> {
    let root: WorkerFileRoot = serde_yaml::from_str(text).map_err(|error| {
        WorkGraphError::new(
            error_code::INVALID_WORKER_FILE_YAML,
            format!("invalid worker file YAML: {error}"),
        )
    })?;
    parse_root(root)
        .map_err(|message| WorkGraphError::new(error_code::INVALID_WORKER_FILE_PAYLOAD, message))
}

fn parse_root(root: WorkerFileRoot) -> Result<WorkerFile, String> {
    if root.version != SUPPORTED_WORKER_FILE_VERSION {
        return Err(format!(
            "version must equal {SUPPORTED_WORKER_FILE_VERSION}, found {}",
            root.version
        ));
    }
    if root.workers.is_empty() {
        return Err("workers must contain at least one worker".to_string());
    }
    if root.workers.len() > MAX_WORKERS {
        return Err(format!(
            "workers must contain at most {MAX_WORKERS} workers, found {}",
            root.workers.len()
        ));
    }

    let mut workers = Vec::with_capacity(root.workers.len());
    let mut seen_worker_ids = BTreeSet::new();
    let mut seen_slot_ids = BTreeSet::new();
    for (index, worker) in root.workers.into_iter().enumerate() {
        let worker = parse_worker(index, worker)?;
        if !seen_worker_ids.insert(worker.worker_id.clone()) {
            return Err(format!(
                "workers[{index}].workerId '{}' is duplicated; worker IDs must be unique",
                worker.worker_id
            ));
        }
        for slot in worker.slot_ids() {
            if !seen_slot_ids.insert(slot.clone()) {
                return Err(format!(
                    "workers[{index}] derives slot ID '{slot}', which is already claimed by \
                     another worker"
                ));
            }
        }
        workers.push(worker);
    }

    Ok(WorkerFile {
        version: SUPPORTED_WORKER_FILE_VERSION,
        workers,
    })
}

fn parse_worker(index: usize, worker: WorkerRoot) -> Result<WorkerDefinition, String> {
    let field = |name: &str| format!("workers[{index}].{name}");
    let worker_id = worker.worker_id;
    if worker_id.is_empty() || worker_id.len() > MAX_WORKER_ID_LEN {
        return Err(format!(
            "{} must be 1 to {MAX_WORKER_ID_LEN} characters",
            field("workerId")
        ));
    }
    // A worker ID must be a stable, path-safe token: slot IDs are derived as
    // `workerId/slotNumber`, so any '/' would make a slot ID ambiguous.
    if !worker_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte))
    {
        return Err(format!(
            "{} must contain only ASCII letters, digits, '-', '.', or '_'",
            field("workerId")
        ));
    }
    if !SUPPORTED_AGENT_PROFILES.contains(&worker.agent_profile.as_str()) {
        return Err(format!(
            "{} must be one of: {}",
            field("agentProfile"),
            SUPPORTED_AGENT_PROFILES.join(", ")
        ));
    }
    if worker.slots == 0 || worker.slots > MAX_WORKER_SLOTS {
        return Err(format!(
            "{} must be between 1 and {MAX_WORKER_SLOTS}, found {}",
            field("slots"),
            worker.slots
        ));
    }
    let lease_duration_seconds = parse_iso8601_duration_seconds(&worker.lease_duration)
        .ok_or_else(|| {
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
    Ok(WorkerDefinition {
        worker_id,
        agent_profile: worker.agent_profile,
        slots: worker.slots,
        lease_duration: worker.lease_duration,
        lease_duration_seconds,
    })
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
