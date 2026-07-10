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

//! Crate-level unit tests for the builder and trait wiring.

use crate::config::{EnterCondition, Op, StateDef};
use crate::{StateMachineSource, StateMachineSourceBuilder};
use drasi_lib::Source;

fn order_states() -> Vec<StateDef> {
    vec![
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
    ]
}

fn build() -> StateMachineSource {
    let mut builder = StateMachineSourceBuilder::new("order-state-source")
        .with_entity_label("OrderState")
        .with_key_field("orderId");
    for state in order_states() {
        builder = builder.with_state(state);
    }
    builder.build().expect("build source")
}

#[test]
fn builder_produces_source() {
    let source = build();
    assert_eq!(source.id(), "order-state-source");
    assert_eq!(source.type_name(), "state-machine");
    assert!(!source.supports_replay());
}

#[test]
fn source_subscribes_to_referenced_queries() {
    let source = build();
    let mut queries = source.subscribed_query_ids();
    queries.sort();
    assert_eq!(queries, vec!["confirmed-orders", "draft-orders"]);
}

#[test]
fn build_fails_without_states() {
    let result = StateMachineSourceBuilder::new("order-state-source").build();
    assert!(result.is_err());
}
