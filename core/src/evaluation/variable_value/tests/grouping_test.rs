// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    sync::Arc,
};

use hashers::jenkins::spooky_hash::SpookyHasher;
use serde_json::json;

use crate::{
    evaluation::functions::aggregation::ValueAccumulator,
    evaluation::variable_value::VariableValue,
    in_memory_index::in_memory_result_index::InMemoryResultIndex,
    interface::{AccumulatorIndex, ResultKey, ResultOwner},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference},
};

fn hash(value: &VariableValue) -> u64 {
    let mut state = SpookyHasher::default();
    value.hash_for_groupby(&mut state);
    state.finish()
}

fn assert_contract(left: &VariableValue, right: &VariableValue, equal: bool) {
    assert_eq!(left.eq_for_groupby(right), equal, "{left:?} vs {right:?}");
    assert_eq!(right.eq_for_groupby(left), equal);
    let a = ResultKey::GroupBy(Arc::new(vec![left.clone(), VariableValue::Null]));
    let b = ResultKey::GroupBy(Arc::new(vec![right.clone(), VariableValue::Null]));
    assert_eq!(a == b, equal, "index-key equality must match grouping");
    if equal {
        assert_eq!(hash(left), hash(right));
        let mut ah = SpookyHasher::default();
        let mut bh = SpookyHasher::default();
        a.hash(&mut ah);
        b.hash(&mut bh);
        assert_eq!(ah.finish(), bh.finish());
    }
}

#[test]
fn grouping_numeric_exact_equivalence_classes() {
    let values = [
        ("zero", VariableValue::from(json!(0))),
        ("zero", VariableValue::from(json!(0.0))),
        ("zero", VariableValue::from(json!(-0.0))),
        ("one", VariableValue::from(json!(1))),
        ("one", VariableValue::from(json!(1.0))),
        ("minus", VariableValue::from(json!(-1))),
        ("minus", VariableValue::from(json!(-1.0))),
        ("fraction", VariableValue::from(json!(1.5))),
        ("fraction", VariableValue::from(json!(1.5))),
        (
            "precise",
            VariableValue::from(json!(9_007_199_254_740_992_i64)),
        ),
        (
            "precise",
            VariableValue::from(json!(9_007_199_254_740_992.0)),
        ),
        (
            "next",
            VariableValue::from(json!(9_007_199_254_740_993_i64)),
        ),
        (
            "next-exact",
            VariableValue::from(json!(9_007_199_254_740_994_i64)),
        ),
        (
            "next-exact",
            VariableValue::from(json!(9_007_199_254_740_994.0)),
        ),
        ("min", VariableValue::from(json!(i64::MIN))),
        ("min", VariableValue::from(json!(i64::MIN as f64))),
        ("max", VariableValue::from(json!(i64::MAX))),
        ("above-max", VariableValue::Integer((1_u64 << 63).into())),
        ("above-max", VariableValue::from(json!(i64::MAX as f64))),
        ("umax", VariableValue::Integer(u64::MAX.into())),
        ("above-umax", VariableValue::from(json!(u64::MAX as f64))),
        ("null", VariableValue::Null),
        ("bool", VariableValue::Bool(true)),
        ("text", VariableValue::String("1".into())),
    ];
    for (a_class, a) in &values {
        for (b_class, b) in &values {
            assert_contract(a, b, a_class == b_class);
        }
    }
    // Do not alter general comparison/arithmetic as a side effect of grouping.
    assert_eq!(
        VariableValue::from(json!(9_007_199_254_740_993_i64)),
        VariableValue::from(json!(9_007_199_254_740_992.0))
    );
}

fn element(id: &str, value: i64) -> VariableValue {
    VariableValue::Element(Arc::new(Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("group-test", id),
            labels: Arc::from([Arc::from("Group")]),
            effective_from: value as u64,
        },
        properties: ElementPropertyMap::from(json!({"value": value})),
    }))
}

#[test]
fn grouping_element_reference_and_compound_values_share_the_contract() {
    let before = element("same", 1);
    let after = element("same", 2);
    let other = element("other", 1);
    assert_ne!(
        before, after,
        "ordinary Element equality includes mutable values"
    );
    assert_contract(&before, &after, true);
    assert_contract(&before, &other, false);
    for (left, right, equal) in [(before, after, true), (element("same", 1), other, false)] {
        let left = VariableValue::List(vec![VariableValue::Object(BTreeMap::from([
            ("element".into(), left),
            ("number".into(), VariableValue::from(json!(1))),
        ]))]);
        let right = VariableValue::List(vec![VariableValue::Object(BTreeMap::from([
            ("number".into(), VariableValue::from(json!(1.0))),
            ("element".into(), right),
        ]))]);
        assert_contract(&left, &right, equal);
    }
    assert_contract(
        &VariableValue::from(json!([1])),
        &VariableValue::from(json!([1, 2])),
        false,
    );
    assert_contract(
        &VariableValue::from(json!({"a": 1})),
        &VariableValue::from(json!({"b": 1.0})),
        false,
    );
    assert_contract(
        &VariableValue::from(json!([1, 2])),
        &VariableValue::from(json!([2.0, 1.0])),
        false,
    );
}

#[test]
fn grouping_nonfinite_float_identity_is_reflexive_without_changing_query_equality() {
    for number in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
        let value = VariableValue::Float(number.into());
        assert_contract(&value, &value.clone(), true);
    }
    assert_contract(
        &VariableValue::Float(f64::INFINITY.into()),
        &VariableValue::Float(f64::NEG_INFINITY.into()),
        false,
    );
}

#[tokio::test]
async fn grouping_numeric_unsigned_max_keeps_negative_accumulator_distinct() {
    let key = |number| {
        ResultKey::GroupBy(Arc::new(vec![VariableValue::List(vec![
            VariableValue::Object(BTreeMap::from([("number".into(), number)])),
        ])]))
    };
    let unsigned = key(VariableValue::Integer(u64::MAX.into()));
    let negative = key(VariableValue::Integer((-1_i64).into()));
    let equivalent_negative = key(VariableValue::Float((-1.0).into()));
    assert_ne!(unsigned, negative);
    assert_eq!(negative, equivalent_negative);
    let index = InMemoryResultIndex::new();
    let owner = ResultOwner::Function(1);
    index
        .set(
            unsigned.clone(),
            owner.clone(),
            Some(ValueAccumulator::Count { value: 10 }),
        )
        .await
        .unwrap();
    index
        .set(
            negative.clone(),
            owner.clone(),
            Some(ValueAccumulator::Count { value: 20 }),
        )
        .await
        .unwrap();
    assert!(matches!(
        index.get(&unsigned, &owner).await.unwrap(),
        Some(ValueAccumulator::Count { value: 10 })
    ));
    assert!(matches!(
        index.get(&equivalent_negative, &owner).await.unwrap(),
        Some(ValueAccumulator::Count { value: 20 })
    ));
    index
        .set(equivalent_negative, owner.clone(), None)
        .await
        .unwrap();
    assert!(index.get(&negative, &owner).await.unwrap().is_none());
    assert!(matches!(
        index.get(&unsigned, &owner).await.unwrap(),
        Some(ValueAccumulator::Count { value: 10 })
    ));
}

#[test]
fn grouping_numeric_encoding_does_not_change_ordinary_integer_hash() {
    #[derive(Default)]
    struct HashBytes(Vec<u8>);
    impl Hasher for HashBytes {
        fn finish(&self) -> u64 {
            0
        }
        fn write(&mut self, bytes: &[u8]) {
            self.0.extend_from_slice(bytes);
        }
    }
    fn bytes(value: impl Hash) -> Vec<u8> {
        let mut hash = HashBytes::default();
        value.hash(&mut hash);
        hash.0
    }
    use crate::evaluation::variable_value::integer::Integer;
    for number in [i64::MIN, -1, 0, 1, i64::MAX] {
        assert_eq!(bytes(Integer::from(number)), bytes(number));
    }
    assert_eq!(bytes(Integer::from(u64::MAX)), bytes(u64::MAX));
    assert_eq!(bytes(Integer::from(1_u64 << 63)), bytes(1_u64 << 63));
    assert_ne!(
        hash(&VariableValue::Integer((-1_i64).into())),
        hash(&VariableValue::Integer(u64::MAX.into())),
    );
}
