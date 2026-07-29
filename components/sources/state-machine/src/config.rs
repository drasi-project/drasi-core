// Copyright 2025 The Drasi Authors.
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

//! Configuration types for the state machine source.
//!
//! A state machine maps live query results to per-entity state transitions. Each
//! [`StateDef`] declares one or more [`EnterCondition`]s; when a subscribed query
//! emits a result whose operation matches and whose entity is currently in an
//! allowed `previous` state, the entity transitions into that state. The source's
//! own id is the id downstream queries subscribe to — there is no separate
//! companion component.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The operation on a query result row that can trigger a transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[schema(as = source::state_machine::Op)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    /// A row was added to the query result set.
    Added,
    /// A row in the query result set was updated.
    Updated,
    /// A row was removed from the query result set.
    Deleted,
}

/// A single condition under which an entity enters a state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[schema(as = source::state_machine::EnterCondition)]
#[serde(rename_all = "camelCase")]
pub struct EnterCondition {
    /// The id of the input query whose results drive this condition.
    pub query: String,
    /// Allowed prior states for the transition to fire.
    ///
    /// * Empty (`[]`) — the condition only matches when the entity has **no**
    ///   current state (initial entry).
    /// * `["*"]` — matches regardless of the entity's current state.
    /// * Otherwise — matches only when the entity's current state is one of the
    ///   listed values.
    #[serde(default)]
    pub previous: Vec<String>,
    /// Handlebars template, rendered against the query result row, that yields the
    /// entity key (e.g. `{{orderId}}`).
    pub key: String,
    /// Which result operations trigger this condition.
    pub ops: Vec<Op>,
}

/// Definition of a single state and the conditions for entering it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[schema(as = source::state_machine::StateDef)]
#[serde(rename_all = "camelCase")]
pub struct StateDef {
    /// The state identifier (e.g. `PAID`).
    pub id: String,
    /// Conditions that cause an entity to enter this state.
    #[serde(default)]
    pub enter: Vec<EnterCondition>,
}

fn default_entity_label() -> String {
    "EntityState".to_string()
}

fn default_key_field() -> String {
    "id".to_string()
}

/// Configuration for the state machine source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[schema(as = source::state_machine::StateMachineSourceConfig)]
#[serde(rename_all = "camelCase")]
pub struct StateMachineSourceConfig {
    /// Label applied to emitted entity-state nodes.
    #[serde(default = "default_entity_label")]
    pub entity_label: String,
    /// Name of the node property that holds the entity key.
    #[serde(default = "default_key_field")]
    pub key_field: String,
    /// The set of states and their entry conditions.
    #[serde(default)]
    pub states: Vec<StateDef>,
}

impl Default for StateMachineSourceConfig {
    fn default() -> Self {
        Self {
            entity_label: default_entity_label(),
            key_field: default_key_field(),
            states: Vec::new(),
        }
    }
}

impl StateMachineSourceConfig {
    /// The de-duplicated set of input query ids referenced by all enter conditions.
    ///
    /// This is exactly the set of queries the source subscribes to (returned from
    /// [`Source::subscribed_query_ids`](drasi_lib::Source::subscribed_query_ids)).
    pub fn referenced_queries(&self) -> Vec<String> {
        let mut seen = Vec::new();
        for state in &self.states {
            for cond in &state.enter {
                if !seen.contains(&cond.query) {
                    seen.push(cond.query.clone());
                }
            }
        }
        seen
    }

    /// Validate the configuration, returning an error describing the first problem.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.entity_label.trim().is_empty() {
            anyhow::bail!("state machine entity_label must not be empty");
        }
        if self.key_field.trim().is_empty() {
            anyhow::bail!("state machine key_field must not be empty");
        }
        if self.states.is_empty() {
            anyhow::bail!("state machine requires at least one state");
        }

        let mut state_ids: Vec<&str> = Vec::new();
        for state in &self.states {
            if state.id.trim().is_empty() {
                anyhow::bail!("state machine state id must not be empty");
            }
            if state_ids.contains(&state.id.as_str()) {
                anyhow::bail!("duplicate state id '{}'", state.id);
            }
            state_ids.push(&state.id);
        }

        for state in &self.states {
            for cond in &state.enter {
                if cond.query.trim().is_empty() {
                    anyhow::bail!("enter condition for state '{}' has empty query", state.id);
                }
                if cond.key.trim().is_empty() {
                    anyhow::bail!(
                        "enter condition for state '{}' (query '{}') has empty key template",
                        state.id,
                        cond.query
                    );
                }
                if cond.ops.is_empty() {
                    anyhow::bail!(
                        "enter condition for state '{}' (query '{}') must list at least one op",
                        state.id,
                        cond.query
                    );
                }
                for prev in &cond.previous {
                    if prev != "*" && !state_ids.contains(&prev.as_str()) {
                        anyhow::bail!(
                            "enter condition for state '{}' references unknown previous state '{}'",
                            state.id,
                            prev
                        );
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StateMachineSourceConfig {
        StateMachineSourceConfig {
            entity_label: "OrderState".to_string(),
            key_field: "orderId".to_string(),
            states: vec![
                StateDef {
                    id: "NEW".to_string(),
                    enter: vec![EnterCondition {
                        query: "draft-orders".to_string(),
                        previous: vec![],
                        key: "{{orderId}}".to_string(),
                        ops: vec![Op::Added],
                    }],
                },
                StateDef {
                    id: "CONFIRMED".to_string(),
                    enter: vec![EnterCondition {
                        query: "confirmed-orders".to_string(),
                        previous: vec!["NEW".to_string()],
                        key: "{{orderId}}".to_string(),
                        ops: vec![Op::Added],
                    }],
                },
            ],
        }
    }

    #[test]
    fn valid_config_passes() {
        sample().validate().unwrap();
    }

    #[test]
    fn referenced_queries_dedup_and_order() {
        assert_eq!(
            sample().referenced_queries(),
            vec!["draft-orders".to_string(), "confirmed-orders".to_string()]
        );
    }

    #[test]
    fn empty_entity_label_fails() {
        let mut cfg = sample();
        cfg.entity_label = String::new();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn no_states_fails() {
        let mut cfg = sample();
        cfg.states.clear();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn unknown_previous_state_fails() {
        let mut cfg = sample();
        cfg.states[1].enter[0].previous = vec!["BOGUS".to_string()];
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn wildcard_previous_is_allowed() {
        let mut cfg = sample();
        cfg.states[1].enter[0].previous = vec!["*".to_string()];
        cfg.validate().unwrap();
    }

    #[test]
    fn duplicate_state_fails() {
        let mut cfg = sample();
        cfg.states[1].id = "NEW".to_string();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn empty_ops_fails() {
        let mut cfg = sample();
        cfg.states[0].enter[0].ops = vec![];
        assert!(cfg.validate().is_err());
    }
}
