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

//! Local mirror of AHP host state and its projection into a Drasi graph.
//!
//! [`AhpMirror`] holds the latest root, session, and automation state and is
//! mutated by snapshots, `action` envelopes (through the official `ahp`
//! reducers), and root session-catalogue notifications. [`GraphProjection`]
//! remembers the last projected graph per [`Scope`] and turns every mirror
//! change into the minimal set of inserts, updates, and deletes, so no AHP
//! action type has to be mapped by hand.

use std::collections::{BTreeMap, HashMap};

use ahp::reducers::{
    apply_action_to_automation, apply_action_to_root, apply_action_to_session, ReduceOutcome,
};
use ahp_types::actions::ActionEnvelope;
use ahp_types::notifications::PartialSessionSummary;
use ahp_types::state::{
    AutomationState, ChangesSummary, RootState, SessionState, SessionSummary, Snapshot,
    SnapshotState,
};
use ahp_types::ROOT_RESOURCE_URI;
use drasi_core::models::SourceChange;
use log::warn;
use serde_json::{Map, Value};

use crate::model::{
    agent_id, input_request_id, insert_status_props, labels, relations, ProjectedElement,
    SessionFilter, AUTOMATIONS_URI,
};

/// Unit of projection. Each scope owns a disjoint set of element IDs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    /// Agents from the root channel.
    Root,
    /// One session: its node, chats, input requests, and their relations.
    Session(String),
    /// The automation catalogue: automations, runs, and `HAS_RUN` relations.
    Automations,
}

/// Mirrored state of a single session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionEntry {
    /// Catalogue summary from `listSessions` / root notifications.
    pub summary: Option<SessionSummary>,
    /// Full session state, present while the session channel is subscribed.
    pub state: Option<SessionState>,
}

impl SessionEntry {
    // Catalogue summaries are kept current by `root/sessionSummaryChanged`
    // (the host's aggregate view); session-channel state is the fallback.
    fn provider(&self) -> Option<&str> {
        self.summary
            .as_ref()
            .map(|s| s.provider.as_str())
            .or_else(|| self.state.as_ref().map(|s| s.provider.as_str()))
    }

    fn status(&self) -> Option<u32> {
        self.summary
            .as_ref()
            .map(|s| s.status)
            .or_else(|| self.state.as_ref().map(|s| s.status))
    }
}

/// Local copy of the AHP host state relevant to the Drasi graph.
#[derive(Debug, Clone)]
pub struct AhpMirror {
    filter: SessionFilter,
    root: Option<RootState>,
    sessions: BTreeMap<String, SessionEntry>,
    automations: Option<AutomationState>,
    channel_seq: HashMap<String, i64>,
}

impl AhpMirror {
    /// Create an empty mirror that projects only sessions selected by `filter`.
    pub fn new(filter: SessionFilter) -> Self {
        Self {
            filter,
            root: None,
            sessions: BTreeMap::new(),
            automations: None,
            channel_seq: HashMap::new(),
        }
    }

    /// The session filter.
    pub fn filter(&self) -> &SessionFilter {
        &self.filter
    }

    /// Root state, if known.
    pub fn root(&self) -> Option<&RootState> {
        self.root.as_ref()
    }

    /// Mirrored sessions keyed by session URI.
    pub fn sessions(&self) -> &BTreeMap<String, SessionEntry> {
        &self.sessions
    }

    /// Automation catalogue, if subscribed.
    pub fn automations(&self) -> Option<&AutomationState> {
        self.automations.as_ref()
    }

    /// Every scope that currently has state.
    pub fn scopes(&self) -> Vec<Scope> {
        let mut scopes = vec![Scope::Root, Scope::Automations];
        scopes.extend(self.sessions.keys().cloned().map(Scope::Session));
        scopes
    }

    /// Whether the session is known and selected by the filter.
    pub fn is_session_included(&self, uri: &str) -> bool {
        self.sessions
            .get(uri)
            .is_some_and(|entry| match (entry.provider(), entry.status()) {
                (Some(provider), Some(status)) => self.filter.includes(provider, status),
                _ => false,
            })
    }

    /// Whether the full session state is mirrored (channel subscribed).
    pub fn has_session_state(&self, uri: &str) -> bool {
        self.sessions.get(uri).is_some_and(|e| e.state.is_some())
    }

    /// Apply a subscription / initialize / reconnect snapshot.
    pub fn apply_snapshot(&mut self, snapshot: &Snapshot) -> Option<Scope> {
        let scope = match &snapshot.state {
            SnapshotState::Root(root) => {
                self.root = Some((**root).clone());
                Scope::Root
            }
            SnapshotState::Session(state) => {
                self.sessions
                    .entry(snapshot.resource.clone())
                    .or_default()
                    .state = Some((**state).clone());
                Scope::Session(snapshot.resource.clone())
            }
            SnapshotState::Automations(state) => {
                self.automations = Some((**state).clone());
                Scope::Automations
            }
            _ => return None,
        };
        self.channel_seq
            .insert(snapshot.resource.clone(), snapshot.from_seq);
        Some(scope)
    }

    /// Apply an `action` envelope. Returns the affected scope, or `None` when
    /// the action is for an unmirrored channel, already covered by a snapshot,
    /// or does not change state.
    pub fn apply_action(&mut self, envelope: &ActionEnvelope) -> Option<Scope> {
        let channel = envelope.channel.as_str();
        let seq = i64::try_from(envelope.server_seq).unwrap_or(i64::MAX);
        match self.channel_seq.get(channel) {
            Some(seen) if seq <= *seen => return None,
            Some(_) => {}
            // Never snapshotted: we cannot apply deltas without a base state.
            None => return None,
        }
        self.channel_seq.insert(channel.to_string(), seq);

        let (outcome, scope) = if channel == ROOT_RESOURCE_URI {
            let root = self.root.as_mut()?;
            (apply_action_to_root(root, &envelope.action), Scope::Root)
        } else if channel == AUTOMATIONS_URI {
            let automations = self.automations.as_mut()?;
            (
                apply_action_to_automation(automations, &envelope.action),
                Scope::Automations,
            )
        } else if channel.starts_with("ahp-session:") {
            let state = self.sessions.get_mut(channel)?.state.as_mut()?;
            (
                apply_action_to_session(state, &envelope.action),
                Scope::Session(channel.to_string()),
            )
        } else {
            return None;
        };

        match outcome {
            ReduceOutcome::Applied => Some(scope),
            ReduceOutcome::Invalid(reason) => {
                warn!(
                    "AHP action #{} on '{channel}' rejected by reducer: {reason:?}",
                    envelope.server_seq
                );
                None
            }
            _ => None,
        }
    }

    /// Insert or replace a session summary (`listSessions`, `root/sessionAdded`).
    pub fn upsert_summary(&mut self, summary: SessionSummary) -> Scope {
        let uri = summary.resource.clone();
        self.sessions.entry(uri.clone()).or_default().summary = Some(summary);
        Scope::Session(uri)
    }

    /// Merge a `root/sessionSummaryChanged` delta. Unknown sessions are ignored.
    pub fn merge_summary(&mut self, uri: &str, changes: &PartialSessionSummary) -> Option<Scope> {
        let summary = self.sessions.get_mut(uri)?.summary.as_mut()?;
        merge_partial_summary(summary, changes);
        Some(Scope::Session(uri.to_string()))
    }

    /// Forget a session (`root/sessionRemoved`).
    pub fn remove_session(&mut self, uri: &str) -> Scope {
        self.sessions.remove(uri);
        self.channel_seq.remove(uri);
        Scope::Session(uri.to_string())
    }

    /// Drop the full state of a session after unsubscribing from it.
    pub fn clear_session_state(&mut self, uri: &str) -> Scope {
        if let Some(entry) = self.sessions.get_mut(uri) {
            entry.state = None;
        }
        self.channel_seq.remove(uri);
        Scope::Session(uri.to_string())
    }

    /// Project one scope into graph elements keyed by element ID.
    pub fn project(&self, scope: &Scope) -> BTreeMap<String, ProjectedElement> {
        let mut out = BTreeMap::new();
        match scope {
            Scope::Root => self.project_root(&mut out),
            Scope::Session(uri) => self.project_session(uri, &mut out),
            Scope::Automations => self.project_automations(&mut out),
        }
        out
    }

    fn project_root(&self, out: &mut BTreeMap<String, ProjectedElement>) {
        let Some(root) = &self.root else { return };
        for agent in &root.agents {
            let mut props = Map::new();
            props.insert("provider".into(), Value::from(agent.provider.clone()));
            props.insert(
                "displayName".into(),
                Value::from(agent.display_name.clone()),
            );
            props.insert("description".into(), Value::from(agent.description.clone()));
            props.insert("modelCount".into(), Value::from(agent.models.len()));
            props.insert(
                "requiresAuth".into(),
                Value::from(
                    agent
                        .protected_resources
                        .as_ref()
                        .is_some_and(|r| !r.is_empty()),
                ),
            );
            insert(
                out,
                ProjectedElement::node(agent_id(&agent.provider), labels::AGENT, props),
            );
        }
    }

    fn project_session(&self, uri: &str, out: &mut BTreeMap<String, ProjectedElement>) {
        if !self.is_session_included(uri) {
            return;
        }
        let Some(entry) = self.sessions.get(uri) else {
            return;
        };
        let summary = entry.summary.as_ref();
        let state = entry.state.as_ref();

        let provider = entry.provider().unwrap_or_default().to_string();
        let status = entry.status().unwrap_or_default();
        let title = state
            .map(|s| s.title.clone())
            .or_else(|| summary.map(|s| s.title.clone()))
            .unwrap_or_default();
        let activity = state
            .and_then(|s| s.activity.clone())
            .or_else(|| summary.and_then(|s| s.activity.clone()));
        let project = state
            .and_then(|s| s.project.as_ref())
            .or_else(|| summary.and_then(|s| s.project.as_ref()));
        let origin = state
            .and_then(|s| s.origin.as_ref())
            .or_else(|| summary.and_then(|s| s.origin.as_ref()));

        let mut props = Map::new();
        props.insert("uri".into(), Value::from(uri));
        props.insert("id".into(), Value::from(channel_id(uri)));
        props.insert("provider".into(), Value::from(provider.clone()));
        props.insert("title".into(), Value::from(title));
        insert_status_props(&mut props, status);
        props.insert("activity".into(), opt_str(activity));
        props.insert(
            "lifecycle".into(),
            state
                .and_then(|s| serde_json::to_value(&s.lifecycle).ok())
                .unwrap_or(Value::Null),
        );
        props.insert(
            "creationError".into(),
            opt_str(
                state
                    .and_then(|s| s.creation_error.as_ref())
                    .map(|e| e.message.clone()),
            ),
        );
        props.insert(
            "createdAt".into(),
            opt_str(summary.map(|s| s.created_at.clone())),
        );
        props.insert(
            "modifiedAt".into(),
            opt_str(summary.map(|s| s.modified_at.clone())),
        );
        insert_changes(&mut props, summary.and_then(|s| s.changes.as_ref()));
        props.insert(
            "projectName".into(),
            opt_str(project.map(|p| p.display_name.clone())),
        );
        props.insert("projectUri".into(), opt_str(project.map(|p| p.uri.clone())));
        props.insert(
            "activeClientCount".into(),
            state
                .map(|s| Value::from(s.active_clients.len()))
                .unwrap_or(Value::Null),
        );
        props.insert(
            "pendingInputCount".into(),
            state
                .map(|s| Value::from(s.input_needed.as_ref().map_or(0, Vec::len)))
                .unwrap_or(Value::Null),
        );

        let default_chat = state
            .and_then(|s| s.default_chat.clone())
            .or_else(|| summary.and_then(|s| s.default_chat.clone()));

        // Chats: full summaries while subscribed, catalogue summaries otherwise.
        let mut chat_count = 0usize;
        if let Some(state) = state {
            for chat in &state.chats {
                chat_count += 1;
                let mut c = Map::new();
                c.insert("uri".into(), Value::from(chat.resource.clone()));
                c.insert("id".into(), Value::from(channel_id(&chat.resource)));
                c.insert("sessionUri".into(), Value::from(uri));
                c.insert("title".into(), Value::from(chat.title.clone()));
                insert_status_props(&mut c, chat.status);
                c.insert("activity".into(), opt_str(chat.activity.clone()));
                c.insert("modifiedAt".into(), Value::from(chat.modified_at.clone()));
                c.insert("provider".into(), opt_str(chat.provider.clone()));
                c.insert(
                    "isDefault".into(),
                    Value::from(default_chat.as_deref() == Some(chat.resource.as_str())),
                );
                insert_changes(&mut c, chat.changes.as_ref());
                add_chat(out, uri, &chat.resource, c);
            }
        } else if let Some(chats) = summary.and_then(|s| s.chats.as_ref()) {
            for chat in chats {
                chat_count += 1;
                let mut c = Map::new();
                c.insert("uri".into(), Value::from(chat.resource.clone()));
                c.insert("id".into(), Value::from(channel_id(&chat.resource)));
                c.insert("sessionUri".into(), Value::from(uri));
                c.insert("title".into(), Value::from(chat.title.clone()));
                match chat.status {
                    Some(status) => insert_status_props(&mut c, status),
                    None => {
                        c.insert("status".into(), Value::Null);
                        c.insert("state".into(), Value::Null);
                    }
                }
                c.insert("activity".into(), Value::Null);
                c.insert("modifiedAt".into(), Value::Null);
                c.insert("provider".into(), opt_str(chat.provider.clone()));
                c.insert(
                    "isDefault".into(),
                    Value::from(default_chat.as_deref() == Some(chat.resource.as_str())),
                );
                insert_changes(&mut c, chat.changes.as_ref());
                add_chat(out, uri, &chat.resource, c);
            }
        }
        props.insert("chatCount".into(), Value::from(chat_count));

        insert(out, ProjectedElement::node(uri, labels::SESSION, props));
        if !provider.is_empty() {
            insert(
                out,
                ProjectedElement::relation(uri, relations::USES_AGENT, &agent_id(&provider)),
            );
        }

        if let Some(Value::Object(origin)) = origin.and_then(|o| serde_json::to_value(o).ok()) {
            if origin.get("kind").and_then(Value::as_str) == Some("automation") {
                if let Some(run) = origin.get("run").and_then(Value::as_str) {
                    insert(
                        out,
                        ProjectedElement::relation(run, relations::CREATED_SESSION, uri),
                    );
                }
            }
        }

        if let Some(requests) = state.and_then(|s| s.input_needed.as_ref()) {
            for request in requests {
                let Ok(Value::Object(v)) = serde_json::to_value(request) else {
                    continue;
                };
                let Some(request_id) = v.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let node_id = input_request_id(uri, request_id);
                let chat = v.get("chat").and_then(Value::as_str);
                let tool_call = v.get("toolCall");
                let mut p = Map::new();
                p.insert("id".into(), Value::from(request_id));
                p.insert("kind".into(), json_field(&v, &["kind"]));
                p.insert("sessionUri".into(), Value::from(uri));
                p.insert("chatUri".into(), opt_str(chat.map(str::to_string)));
                p.insert("turnId".into(), json_field(&v, &["turnId"]));
                p.insert("message".into(), json_field(&v, &["request", "message"]));
                p.insert(
                    "questionCount".into(),
                    v.get("request")
                        .and_then(|r| r.get("questions"))
                        .and_then(Value::as_array)
                        .map(|q| Value::from(q.len()))
                        .unwrap_or(Value::Null),
                );
                p.insert(
                    "toolName".into(),
                    tool_call
                        .and_then(|t| t.get("toolName"))
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                p.insert(
                    "toolDisplayName".into(),
                    tool_call
                        .and_then(|t| t.get("displayName"))
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                insert(
                    out,
                    ProjectedElement::node(node_id.clone(), labels::INPUT_REQUEST, p),
                );
                insert(
                    out,
                    ProjectedElement::relation(uri, relations::NEEDS_INPUT, &node_id),
                );
                if let Some(chat) = chat {
                    insert(
                        out,
                        ProjectedElement::relation(&node_id, relations::IN_CHAT, chat),
                    );
                }
            }
        }
    }

    fn project_automations(&self, out: &mut BTreeMap<String, ProjectedElement>) {
        let Some(automations) = &self.automations else {
            return;
        };
        for entry in &automations.entries {
            let mut a = Map::new();
            a.insert("uri".into(), Value::from(entry.resource.clone()));
            a.insert("id".into(), Value::from(channel_id(&entry.resource)));
            a.insert("title".into(), Value::from(entry.definition.title.clone()));
            a.insert("enabled".into(), Value::from(entry.definition.enabled));
            a.insert(
                "triggerCount".into(),
                Value::from(entry.definition.triggers.len()),
            );
            a.insert("nextRunAt".into(), opt_str(entry.next_run_at.clone()));
            a.insert(
                "runCount".into(),
                entry.run_count.map(Value::from).unwrap_or(Value::Null),
            );
            a.insert("createdAt".into(), Value::from(entry.created_at.clone()));
            a.insert("modifiedAt".into(), Value::from(entry.modified_at.clone()));
            insert(
                out,
                ProjectedElement::node(entry.resource.clone(), labels::AUTOMATION, a),
            );

            for run in &entry.runs {
                let lifecycle = serde_json::to_value(&run.lifecycle).unwrap_or(Value::Null);
                let origin = serde_json::to_value(&run.origin).unwrap_or(Value::Null);
                let mut r = Map::new();
                r.insert("uri".into(), Value::from(run.resource.clone()));
                r.insert("id".into(), Value::from(channel_id(&run.resource)));
                r.insert("automationUri".into(), Value::from(run.automation.clone()));
                r.insert("status".into(), json_value_field(&lifecycle, &["status"]));
                r.insert("originKind".into(), json_value_field(&origin, &["kind"]));
                r.insert(
                    "error".into(),
                    json_value_field(&lifecycle, &["error", "message"]),
                );
                r.insert(
                    "primarySessionUri".into(),
                    opt_str(run.primary_session.clone()),
                );
                r.insert("sessionCount".into(), Value::from(run.session_count));
                insert(
                    out,
                    ProjectedElement::node(run.resource.clone(), labels::AUTOMATION_RUN, r),
                );
                insert(
                    out,
                    ProjectedElement::relation(&entry.resource, relations::HAS_RUN, &run.resource),
                );
            }
        }
    }
}

fn add_chat(
    out: &mut BTreeMap<String, ProjectedElement>,
    session_uri: &str,
    chat_uri: &str,
    props: Map<String, Value>,
) {
    insert(out, ProjectedElement::node(chat_uri, labels::CHAT, props));
    insert(
        out,
        ProjectedElement::relation(session_uri, relations::HAS_CHAT, chat_uri),
    );
}

fn insert(out: &mut BTreeMap<String, ProjectedElement>, element: ProjectedElement) {
    out.insert(element.id.clone(), element);
}

fn insert_changes(props: &mut Map<String, Value>, changes: Option<&ChangesSummary>) {
    let get = |f: fn(&ChangesSummary) -> Option<i64>| {
        changes.and_then(f).map(Value::from).unwrap_or(Value::Null)
    };
    props.insert("additions".into(), get(|c| c.additions));
    props.insert("deletions".into(), get(|c| c.deletions));
    props.insert("filesChanged".into(), get(|c| c.files));
}

fn opt_str(value: Option<String>) -> Value {
    value.map(Value::from).unwrap_or(Value::Null)
}

fn json_field(map: &Map<String, Value>, path: &[&str]) -> Value {
    let Some((first, rest)) = path.split_first() else {
        return Value::Null;
    };
    map.get(*first)
        .map(|v| json_value_field(v, rest))
        .unwrap_or(Value::Null)
}

fn json_value_field(value: &Value, path: &[&str]) -> Value {
    let mut current = value;
    for key in path {
        match current.get(key) {
            Some(next) => current = next,
            None => return Value::Null,
        }
    }
    current.clone()
}

/// The identifier part of a channel URI (`ahp-session:/abc` -> `abc`).
pub fn channel_id(uri: &str) -> &str {
    let rest = uri.split_once(':').map_or(uri, |(_, rest)| rest);
    rest.trim_start_matches('/')
}

/// Apply a partial session summary to a full one.
pub fn merge_partial_summary(summary: &mut SessionSummary, changes: &PartialSessionSummary) {
    macro_rules! set {
        ($field:ident) => {
            if let Some(v) = &changes.$field {
                summary.$field = v.clone();
            }
        };
    }
    macro_rules! set_opt {
        ($field:ident) => {
            if let Some(v) = &changes.$field {
                summary.$field = Some(v.clone());
            }
        };
    }
    set!(provider);
    set!(title);
    set!(status);
    set!(resource);
    set!(created_at);
    set!(modified_at);
    set_opt!(activity);
    set_opt!(origin);
    set_opt!(project);
    set_opt!(working_directories);
    set_opt!(annotations);
    set_opt!(changes);
    set_opt!(meta);
    set_opt!(chats);
    set_opt!(default_chat);
}

/// One element-level change produced by diffing projections.
#[derive(Debug, Clone, PartialEq)]
pub enum ElementChange {
    /// A new element.
    Insert(ProjectedElement),
    /// An element whose properties changed.
    Update(ProjectedElement),
    /// An element that no longer exists.
    Delete(ProjectedElement),
}

impl ElementChange {
    /// The element this change refers to.
    pub fn element(&self) -> &ProjectedElement {
        match self {
            Self::Insert(e) | Self::Update(e) | Self::Delete(e) => e,
        }
    }

    /// Ordering rank that keeps relations consistent with their endpoints:
    /// relation deletes, node deletes, node upserts, relation upserts.
    fn rank(&self) -> u8 {
        match (self, self.element().is_relation()) {
            (Self::Delete(_), true) => 0,
            (Self::Delete(_), false) => 1,
            (_, false) => 2,
            (_, true) => 3,
        }
    }

    /// Convert into a Drasi [`SourceChange`].
    pub fn to_source_change(&self, source_id: &str, effective_from: u64) -> SourceChange {
        match self {
            Self::Insert(e) => e.to_insert(source_id, effective_from),
            Self::Update(e) => e.to_update(source_id, effective_from),
            Self::Delete(e) => e.to_delete(source_id, effective_from),
        }
    }
}

/// Last emitted graph, per scope. Diffs the mirror against it on demand.
#[derive(Debug, Default, Clone)]
pub struct GraphProjection {
    scopes: HashMap<Scope, BTreeMap<String, ProjectedElement>>,
}

impl GraphProjection {
    /// Create an empty projection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Re-project one scope and return the changes since the last refresh.
    pub fn refresh(&mut self, mirror: &AhpMirror, scope: &Scope) -> Vec<ElementChange> {
        let new = mirror.project(scope);
        let old = if new.is_empty() {
            self.scopes.remove(scope).unwrap_or_default()
        } else {
            self.scopes
                .insert(scope.clone(), new.clone())
                .unwrap_or_default()
        };
        let mut changes = diff(&old, &new);
        sort_changes(&mut changes);
        changes
    }

    /// Re-project every scope known to either the mirror or this projection
    /// (used after a full resynchronisation).
    pub fn refresh_all(&mut self, mirror: &AhpMirror) -> Vec<ElementChange> {
        let mut scopes: Vec<Scope> = self.scopes.keys().cloned().collect();
        scopes.extend(mirror.scopes());
        scopes.sort();
        scopes.dedup();
        let mut changes = Vec::new();
        for scope in scopes {
            changes.extend(self.refresh(mirror, &scope));
        }
        sort_changes(&mut changes);
        changes
    }

    /// Iterate every currently projected element.
    pub fn elements(&self) -> impl Iterator<Item = &ProjectedElement> {
        self.scopes.values().flat_map(BTreeMap::values)
    }

    /// Number of projected elements.
    pub fn len(&self) -> usize {
        self.scopes.values().map(BTreeMap::len).sum()
    }

    /// Whether no elements are projected.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn diff(
    old: &BTreeMap<String, ProjectedElement>,
    new: &BTreeMap<String, ProjectedElement>,
) -> Vec<ElementChange> {
    let mut changes = Vec::new();
    for (id, element) in old {
        if !new.contains_key(id) {
            changes.push(ElementChange::Delete(element.clone()));
        }
    }
    for (id, element) in new {
        match old.get(id) {
            None => changes.push(ElementChange::Insert(element.clone())),
            Some(previous) if previous != element => {
                changes.push(ElementChange::Update(element.clone()))
            }
            Some(_) => {}
        }
    }
    changes
}

fn sort_changes(changes: &mut [ElementChange]) {
    changes.sort_by_key(ElementChange::rank);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::status;
    use ahp_types::state::SnapshotState;
    use serde_json::json;

    fn session_state(title: &str, status: u32) -> SessionState {
        serde_json::from_value(json!({
            "provider": "copilot",
            "title": title,
            "status": status,
            "lifecycle": "ready",
            "activeClients": [],
            "chats": [{
                "resource": "ahp-chat:/c1",
                "title": "Main",
                "status": status,
                "modifiedAt": "2026-01-01T00:00:00Z"
            }]
        }))
        .expect("valid session state")
    }

    fn summary(uri: &str, status: u32) -> SessionSummary {
        serde_json::from_value(json!({
            "provider": "copilot",
            "title": "Fix bug",
            "status": status,
            "resource": uri,
            "createdAt": "2026-01-01T00:00:00Z",
            "modifiedAt": "2026-01-01T00:00:00Z",
            "changes": {"additions": 3, "deletions": 1, "files": 2}
        }))
        .expect("valid summary")
    }

    fn snapshot(resource: &str, state: SnapshotState, from_seq: i64) -> Snapshot {
        Snapshot {
            resource: resource.into(),
            state,
            from_seq,
        }
    }

    #[test]
    fn channel_id_strips_scheme() {
        assert_eq!(channel_id("ahp-session:/abc"), "abc");
        assert_eq!(channel_id("ahp-automations://"), "");
        assert_eq!(channel_id("plain"), "plain");
    }

    #[test]
    fn session_projection_and_reducer_diff() {
        let uri = "ahp-session:/s1";
        let mut mirror = AhpMirror::new(SessionFilter::default());
        mirror.upsert_summary(summary(uri, status::IDLE));
        mirror.apply_snapshot(&snapshot(
            uri,
            SnapshotState::Session(Box::new(session_state("Fix bug", status::IDLE))),
            10,
        ));
        let mut projection = GraphProjection::new();
        let initial = projection.refresh_all(&mirror);
        // Session + chat nodes, USES_AGENT + HAS_CHAT relations.
        assert_eq!(initial.len(), 4);
        assert!(initial
            .iter()
            .all(|c| matches!(c, ElementChange::Insert(_))));
        // Nodes come before relations.
        assert!(!initial[0].element().is_relation());
        assert!(initial[3].element().is_relation());

        // Stale action is ignored.
        let stale: ActionEnvelope = serde_json::from_value(json!({
            "channel": uri, "serverSeq": 10,
            "action": {"type": "session/titleChanged", "title": "old"}
        }))
        .expect("envelope");
        assert_eq!(mirror.apply_action(&stale), None);

        let rename: ActionEnvelope = serde_json::from_value(json!({
            "channel": uri, "serverSeq": 11,
            "action": {"type": "session/titleChanged", "title": "Renamed"}
        }))
        .expect("envelope");
        let scope = mirror.apply_action(&rename).expect("applied");
        let changes = projection.refresh(&mirror, &scope);
        assert_eq!(changes.len(), 1);
        let ElementChange::Update(element) = &changes[0] else {
            panic!("expected update");
        };
        assert_eq!(element.properties["title"], json!("Renamed"));
        assert_eq!(element.properties["additions"], json!(3));
    }

    #[test]
    fn input_request_projection() {
        let uri = "ahp-session:/s1";
        let mut mirror = AhpMirror::new(SessionFilter::default());
        mirror.upsert_summary(summary(uri, status::IN_PROGRESS));
        mirror.apply_snapshot(&snapshot(
            uri,
            SnapshotState::Session(Box::new(session_state("t", status::IN_PROGRESS))),
            1,
        ));
        let mut projection = GraphProjection::new();
        projection.refresh_all(&mirror);

        let add: ActionEnvelope = serde_json::from_value(json!({
            "channel": uri, "serverSeq": 2,
            "action": {"type": "session/inputNeededSet", "request": {
                "kind": "chatInput", "id": "q1", "chat": "ahp-chat:/c1",
                "request": {"id": "q1", "message": "Which branch?"}
            }}
        }))
        .expect("envelope");
        let scope = mirror.apply_action(&add).expect("applied");
        let changes = projection.refresh(&mirror, &scope);
        let inserted: Vec<_> = changes
            .iter()
            .filter_map(|c| match c {
                ElementChange::Insert(e) => Some(e.label),
                _ => None,
            })
            .collect();
        assert!(inserted.contains(&labels::INPUT_REQUEST));
        assert!(inserted.contains(&relations::NEEDS_INPUT));
        assert!(inserted.contains(&relations::IN_CHAT));
        let request = projection
            .elements()
            .find(|e| e.label == labels::INPUT_REQUEST)
            .expect("input request");
        assert_eq!(request.properties["message"], json!("Which branch?"));
        assert_eq!(request.properties["kind"], json!("chatInput"));

        let remove: ActionEnvelope = serde_json::from_value(json!({
            "channel": uri, "serverSeq": 3,
            "action": {"type": "session/inputNeededRemoved", "id": "q1"}
        }))
        .expect("envelope");
        let scope = mirror.apply_action(&remove).expect("applied");
        let changes = projection.refresh(&mirror, &scope);
        let deletes = changes
            .iter()
            .filter(|c| matches!(c, ElementChange::Delete(_)))
            .count();
        assert_eq!(deletes, 3);
        // Relation deletes precede the node delete.
        assert!(changes[0].element().is_relation());
    }

    #[test]
    fn archived_sessions_are_removed_when_excluded() {
        let uri = "ahp-session:/s1";
        let mut mirror = AhpMirror::new(SessionFilter {
            providers: vec![],
            include_archived: false,
        });
        mirror.upsert_summary(summary(uri, status::IDLE));
        let mut projection = GraphProjection::new();
        assert_eq!(projection.refresh_all(&mirror).len(), 2);

        let changes = PartialSessionSummary {
            status: Some(status::IDLE | status::IS_ARCHIVED),
            ..Default::default()
        };
        let scope = mirror.merge_summary(uri, &changes).expect("known");
        let changes = projection.refresh(&mirror, &scope);
        assert_eq!(changes.len(), 2);
        assert!(changes
            .iter()
            .all(|c| matches!(c, ElementChange::Delete(_))));
        assert!(projection.is_empty());
    }

    #[test]
    fn summary_chats_used_without_subscription() {
        let uri = "ahp-session:/s1";
        let mut mirror = AhpMirror::new(SessionFilter::default());
        let mut s = summary(uri, status::IDLE);
        s.chats = Some(vec![serde_json::from_value(json!({
            "resource": "ahp-chat:/c9", "title": "Side", "status": 1
        }))
        .expect("chat")]);
        mirror.upsert_summary(s);
        let elements = mirror.project(&Scope::Session(uri.into()));
        assert!(elements.contains_key("ahp-chat:/c9"));
        assert_eq!(elements[uri].properties["chatCount"], json!(1));
        assert_eq!(elements[uri].properties["pendingInputCount"], Value::Null);
    }

    #[test]
    fn remove_session_deletes_everything() {
        let uri = "ahp-session:/s1";
        let mut mirror = AhpMirror::new(SessionFilter::default());
        mirror.upsert_summary(summary(uri, status::IDLE));
        let mut projection = GraphProjection::new();
        projection.refresh_all(&mirror);
        let scope = mirror.remove_session(uri);
        let changes = projection.refresh(&mirror, &scope);
        assert_eq!(changes.len(), 2);
        assert!(projection.is_empty());
    }
}
