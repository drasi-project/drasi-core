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

//! The state machine engine: pure, IO-free transition logic.
//!
//! The engine holds the current state of every entity and, given a
//! [`QueryResult`], computes the set of state transitions that should occur. It
//! performs no persistence or dispatch — those concerns live in the source
//! component.

use std::collections::HashMap;

use anyhow::{Context, Result};
use drasi_lib::channels::{QueryResult, ResultDiff};
use handlebars::Handlebars;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::{Op, StateMachineSourceConfig};

/// A persisted, serializable record describing an entity's current state. This is
/// both the engine's in-memory representation and the value stored in the state
/// store; it is self-describing so the source can rebuild nodes during bootstrap
/// without access to the full config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityRecord {
    /// The entity key (used as the graph node `element_id`).
    pub key: String,
    /// Name of the node property that holds the entity key.
    pub key_field: String,
    /// Label applied to the emitted node.
    pub label: String,
    /// The entity's current state id.
    pub state: String,
    /// The state the entity was in immediately before `state`, if any.
    pub previous_state: Option<String>,
    /// Wall-clock time (epoch millis) when the entity entered `state`.
    pub entered_at: i64,
    /// Pass-through fields from the triggering query result row.
    #[serde(default)]
    pub fields: Map<String, Value>,
}

impl EntityRecord {
    /// Build the full node property object for this entity: the pass-through
    /// fields plus the state-machine metadata (`<key_field>`, `state`,
    /// `previousState`, `enteredAt`). Metadata keys take precedence on collision.
    pub fn node_properties(&self) -> Value {
        let mut props = self.fields.clone();
        props.insert(self.key_field.clone(), Value::String(self.key.clone()));
        props.insert("state".to_string(), Value::String(self.state.clone()));
        props.insert(
            "previousState".to_string(),
            self.previous_state
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        props.insert(
            "enteredAt".to_string(),
            Value::Number(self.entered_at.into()),
        );
        Value::Object(props)
    }
}

/// A compiled enter condition with its target state resolved.
struct CompiledCondition {
    target_state: String,
    query: String,
    previous: Vec<String>,
    ops: Vec<Op>,
    template_name: String,
}

/// The state machine engine.
pub struct StateMachine {
    label: String,
    key_field: String,
    conditions: Vec<CompiledCondition>,
    handlebars: Handlebars<'static>,
    entities: HashMap<String, EntityRecord>,
}

impl StateMachine {
    /// Build an engine from the source config, compiling all key templates.
    pub fn new(config: &StateMachineSourceConfig) -> Result<Self> {
        let mut handlebars = Handlebars::new();
        handlebars.set_strict_mode(false);
        let mut conditions = Vec::new();

        for state in &config.states {
            for (idx, cond) in state.enter.iter().enumerate() {
                let template_name = format!("{}__{idx}", state.id);
                handlebars
                    .register_template_string(&template_name, &cond.key)
                    .with_context(|| {
                        format!(
                            "invalid key template '{}' for state '{}'",
                            cond.key, state.id
                        )
                    })?;
                conditions.push(CompiledCondition {
                    target_state: state.id.clone(),
                    query: cond.query.clone(),
                    previous: cond.previous.clone(),
                    ops: cond.ops.clone(),
                    template_name,
                });
            }
        }

        Ok(Self {
            label: config.entity_label.clone(),
            key_field: config.key_field.clone(),
            conditions,
            handlebars,
            entities: HashMap::new(),
        })
    }

    /// Seed the engine's in-memory state from previously persisted records.
    pub fn load(&mut self, records: impl IntoIterator<Item = EntityRecord>) {
        for record in records {
            self.entities.insert(record.key.clone(), record);
        }
    }

    /// Current number of tracked entities (primarily for testing/metrics).
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// Look up the current record for an entity (primarily for testing).
    pub fn get(&self, key: &str) -> Option<&EntityRecord> {
        self.entities.get(key)
    }

    /// Process a query result, applying any matching transitions and returning the
    /// updated [`EntityRecord`]s (one per entity that changed state).
    ///
    /// Transitions are evaluated in config order, so multiple conditions in a
    /// single result can cascade an entity through several states.
    pub fn process(&mut self, result: &QueryResult) -> Vec<EntityRecord> {
        let mut changed: Vec<EntityRecord> = Vec::new();

        for diff in &result.results {
            let Some((op, row)) = diff_op_and_row(diff) else {
                continue;
            };

            for cond in &self.conditions {
                if cond.query != result.query_id || !cond.ops.contains(&op) {
                    continue;
                }

                let key = match self.handlebars.render(&cond.template_name, &row) {
                    Ok(k) => k.trim().to_string(),
                    Err(e) => {
                        log::warn!(
                            "state machine: failed to render key template for state '{}': {e}",
                            cond.target_state
                        );
                        continue;
                    }
                };
                if key.is_empty() {
                    continue;
                }

                let current = self.entities.get(&key).map(|r| r.state.clone());

                if !previous_matches(&cond.previous, current.as_deref()) {
                    continue;
                }
                // Idempotent: already in the target state.
                if current.as_deref() == Some(cond.target_state.as_str()) {
                    continue;
                }

                // Merge pass-through fields: retain fields accumulated from earlier
                // transitions and overlay the triggering row's fields on top. This
                // keeps context (e.g. customer, amount captured at the first stage)
                // available even when later stage queries return only the key.
                let mut fields = self
                    .entities
                    .get(&key)
                    .map(|r| r.fields.clone())
                    .unwrap_or_default();
                if let Value::Object(map) = &row {
                    for (k, v) in map {
                        fields.insert(k.clone(), v.clone());
                    }
                }

                let record = EntityRecord {
                    key: key.clone(),
                    key_field: self.key_field.clone(),
                    label: self.label.clone(),
                    state: cond.target_state.clone(),
                    previous_state: current,
                    entered_at: chrono::Utc::now().timestamp_millis(),
                    fields,
                };

                self.entities.insert(key, record.clone());

                // Replace any earlier change for the same entity in this batch so
                // only the final state is emitted.
                changed.retain(|r| r.key != record.key);
                changed.push(record);
            }
        }

        changed
    }
}

/// Returns whether `previous` guard allows a transition given the entity's
/// `current` state.
fn previous_matches(previous: &[String], current: Option<&str>) -> bool {
    if previous.iter().any(|p| p == "*") {
        return true;
    }
    match current {
        None => previous.is_empty(),
        Some(state) => previous.iter().any(|p| p == state),
    }
}

/// Map a [`ResultDiff`] to its operation and the row to template against.
fn diff_op_and_row(diff: &ResultDiff) -> Option<(Op, Value)> {
    match diff {
        ResultDiff::Add { data, .. } => Some((Op::Added, data.clone())),
        ResultDiff::Update { after, .. } => Some((Op::Updated, after.clone())),
        ResultDiff::Delete { data, .. } => Some((Op::Deleted, data.clone())),
        ResultDiff::Aggregation { after, .. } => Some((Op::Updated, after.clone())),
        ResultDiff::Noop => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EnterCondition, StateDef};
    use serde_json::json;

    fn config() -> StateMachineSourceConfig {
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
                StateDef {
                    id: "PAID".to_string(),
                    enter: vec![EnterCondition {
                        query: "paid-orders".to_string(),
                        previous: vec!["CONFIRMED".to_string()],
                        key: "{{orderId}}".to_string(),
                        ops: vec![Op::Added],
                    }],
                },
            ],
        }
    }

    fn result(query_id: &str, diff: ResultDiff) -> QueryResult {
        QueryResult::new(
            query_id.to_string(),
            1,
            chrono::Utc::now(),
            vec![diff],
            Default::default(),
        )
    }

    fn add(data: serde_json::Value) -> ResultDiff {
        ResultDiff::Add {
            data,
            row_signature: 0,
        }
    }

    #[test]
    fn initial_entry_from_no_state() {
        let mut sm = StateMachine::new(&config()).unwrap();
        let changed = sm.process(&result("draft-orders", add(json!({"orderId": "123"}))));
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].state, "NEW");
        assert_eq!(changed[0].previous_state, None);
        assert_eq!(changed[0].key, "123");
        assert_eq!(changed[0].label, "OrderState");
    }

    #[test]
    fn guarded_transition_requires_previous_state() {
        let mut sm = StateMachine::new(&config()).unwrap();
        // paid-orders arrives before the order is CONFIRMED -> no transition.
        let changed = sm.process(&result("paid-orders", add(json!({"orderId": "123"}))));
        assert!(changed.is_empty());
        assert!(sm.get("123").is_none());
    }

    #[test]
    fn full_progression() {
        let mut sm = StateMachine::new(&config()).unwrap();
        sm.process(&result("draft-orders", add(json!({"orderId": "1"}))));
        sm.process(&result("confirmed-orders", add(json!({"orderId": "1"}))));
        let changed = sm.process(&result("paid-orders", add(json!({"orderId": "1"}))));
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].state, "PAID");
        assert_eq!(changed[0].previous_state.as_deref(), Some("CONFIRMED"));
    }

    #[test]
    fn transition_is_idempotent() {
        let mut sm = StateMachine::new(&config()).unwrap();
        sm.process(&result("draft-orders", add(json!({"orderId": "1"}))));
        let again = sm.process(&result("draft-orders", add(json!({"orderId": "1"}))));
        assert!(again.is_empty());
    }

    #[test]
    fn pass_through_fields_are_retained() {
        let mut sm = StateMachine::new(&config()).unwrap();
        let changed = sm.process(&result(
            "draft-orders",
            add(json!({"orderId": "1", "customer": "Acme", "total": 42})),
        ));
        let props = changed[0].node_properties();
        assert_eq!(props["customer"], json!("Acme"));
        assert_eq!(props["total"], json!(42));
        assert_eq!(props["orderId"], json!("1"));
        assert_eq!(props["state"], json!("NEW"));
        assert_eq!(props["previousState"], Value::Null);
        assert!(props["enteredAt"].is_number());
    }

    #[test]
    fn fields_merge_across_transitions() {
        let mut sm = StateMachine::new(&config()).unwrap();
        // NEW carries customer; later stages return only the key.
        sm.process(&result(
            "draft-orders",
            add(json!({"orderId": "1", "customer": "Acme"})),
        ));
        sm.process(&result("confirmed-orders", add(json!({"orderId": "1"}))));
        let changed = sm.process(&result("paid-orders", add(json!({"orderId": "1"}))));
        assert_eq!(changed[0].state, "PAID");
        // customer captured at NEW is still present after two more transitions.
        let props = changed[0].node_properties();
        assert_eq!(props["customer"], json!("Acme"));
    }

    #[test]
    fn load_seeds_state_for_guards() {
        let mut sm = StateMachine::new(&config()).unwrap();
        sm.load([EntityRecord {
            key: "1".to_string(),
            key_field: "orderId".to_string(),
            label: "OrderState".to_string(),
            state: "CONFIRMED".to_string(),
            previous_state: Some("NEW".to_string()),
            entered_at: 0,
            fields: Map::new(),
        }]);
        // After reload, a paid-orders result can advance because state is CONFIRMED.
        let changed = sm.process(&result("paid-orders", add(json!({"orderId": "1"}))));
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].state, "PAID");
    }

    #[test]
    fn op_must_match() {
        let mut sm = StateMachine::new(&config()).unwrap();
        // draft-orders only triggers on `added`; a delete should not enter NEW.
        let changed = sm.process(&result(
            "draft-orders",
            ResultDiff::Delete {
                data: json!({"orderId": "1"}),
                row_signature: 0,
            },
        ));
        assert!(changed.is_empty());
    }

    #[test]
    fn wildcard_previous_matches_any_state() {
        let mut cfg = config();
        cfg.states.push(StateDef {
            id: "CANCELLED".to_string(),
            enter: vec![EnterCondition {
                query: "cancelled-orders".to_string(),
                previous: vec!["*".to_string()],
                key: "{{orderId}}".to_string(),
                ops: vec![Op::Added],
            }],
        });
        let mut sm = StateMachine::new(&cfg).unwrap();
        sm.process(&result("draft-orders", add(json!({"orderId": "1"}))));
        let changed = sm.process(&result("cancelled-orders", add(json!({"orderId": "1"}))));
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].state, "CANCELLED");
        assert_eq!(changed[0].previous_state.as_deref(), Some("NEW"));
    }
}
