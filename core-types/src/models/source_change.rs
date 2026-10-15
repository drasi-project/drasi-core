// Copyright 2024 The Drasi Authors.
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

use crate::interface::FutureElementRef;

use serde::{Deserialize, Serialize};

use super::{Element, ElementMetadata, ElementReference, ElementTimestamp};

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum SourceChange {
    Insert { element: Element },
    Update { element: Element },
    Delete { metadata: ElementMetadata },
    Future { future_ref: FutureElementRef },
}

impl SourceChange {
    pub fn get_reference(&self) -> &ElementReference {
        match self {
            SourceChange::Insert { element } => element.get_reference(),
            SourceChange::Update { element } => element.get_reference(),
            SourceChange::Delete { metadata } => &metadata.reference,
            SourceChange::Future { future_ref } => &future_ref.element_ref,
        }
    }

    pub fn get_transaction_time(&self) -> ElementTimestamp {
        match self {
            SourceChange::Insert { element } => element.get_effective_from(),
            SourceChange::Update { element } => element.get_effective_from(),
            SourceChange::Delete { metadata } => metadata.effective_from,
            SourceChange::Future { future_ref } => future_ref.original_time,
        }
    }

    pub fn get_realtime(&self) -> ElementTimestamp {
        match self {
            SourceChange::Insert { element } => element.get_effective_from(),
            SourceChange::Update { element } => element.get_effective_from(),
            SourceChange::Delete { metadata } => metadata.effective_from,
            SourceChange::Future { future_ref } => future_ref.due_time,
        }
    }
}

#[cfg(test)]
mod serde_roundtrip_tests {
    //! T1 — codec round-trip fidelity (regression for issue #602).
    //!
    //! After #602, `SourceChange` payloads cross the cdylib FFI boundary as
    //! serialized MessagePack bytes (via `rmp-serde`) instead of as reinterpreted
    //! `repr(Rust)` opaque pointers. These tests pin the lossless round-trip that
    //! the serialized transfer depends on. They fail to even compile against the
    //! pre-fix code (the model types did not derive `serde`), and they exercise
    //! edge values (`NaN`/`Infinity`) that the legacy `ElementValue -> serde_json`
    //! projection cannot represent (it `unwrap()`s on `Number::from_f64`).

    use super::*;
    use crate::interface::FutureElementRef;
    use crate::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue,
    };
    use chrono::{DateTime, FixedOffset, NaiveDate};
    use ordered_float::OrderedFloat;
    use std::sync::Arc;

    fn roundtrip(change: &SourceChange) -> SourceChange {
        // Mirror the production FFI wire format: named-map encoding
        // (`to_vec_named`), which tolerates skipped fields and field-count changes.
        let bytes = rmp_serde::to_vec_named(change).expect("serialize SourceChange");
        rmp_serde::from_slice(&bytes).expect("deserialize SourceChange")
    }

    fn rich_properties() -> ElementPropertyMap {
        let mut props = ElementPropertyMap::new();
        props.insert("null", ElementValue::Null);
        props.insert("bool", ElementValue::Bool(true));
        props.insert("int", ElementValue::Integer(-9_223_372_036_854_775_808));
        props.insert("float", ElementValue::Float(OrderedFloat(1.234_567_89)));
        props.insert("string", ElementValue::String(Arc::from("héllo 世界")));
        props.insert(
            "list",
            ElementValue::List(vec![
                ElementValue::Integer(1),
                ElementValue::String(Arc::from("two")),
                ElementValue::Null,
            ]),
        );
        let mut nested = ElementPropertyMap::new();
        nested.insert("inner", ElementValue::Bool(false));
        props.insert("object", ElementValue::Object(nested));
        props.insert(
            "local_dt",
            ElementValue::LocalDateTime(
                NaiveDate::from_ymd_opt(2024, 6, 15)
                    .unwrap()
                    .and_hms_opt(13, 45, 30)
                    .unwrap(),
            ),
        );
        props.insert(
            "zoned_dt",
            ElementValue::ZonedDateTime(
                DateTime::<FixedOffset>::parse_from_rfc3339("2024-06-15T13:45:30+02:00").unwrap(),
            ),
        );
        props
    }

    fn node(properties: ElementPropertyMap) -> Element {
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("src-1", "vehicles:A1234"),
                labels: Arc::from(vec![Arc::from("vehicles"), Arc::from("Vehicle")]),
                effective_from: 1_771_000_000_000,
            },
            properties,
        }
    }

    #[test]
    fn insert_node_with_all_value_kinds_roundtrips() {
        let change = SourceChange::Insert {
            element: node(rich_properties()),
        };
        assert_eq!(roundtrip(&change), change);
    }

    #[test]
    fn float_nan_and_infinity_roundtrip_losslessly() {
        // The legacy ElementValue -> serde_json projection panics on NaN/Inf
        // (Number::from_f64(..).unwrap()). The binary codec must preserve them.
        for special in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut props = ElementPropertyMap::new();
            props.insert("f", ElementValue::Float(OrderedFloat(special)));
            let change = SourceChange::Insert {
                element: node(props),
            };
            let back = roundtrip(&change);
            let SourceChange::Insert { element } = &back else {
                panic!("expected Insert");
            };
            let ElementValue::Float(f) = element.get_property("f") else {
                panic!("expected Float");
            };
            if special.is_nan() {
                assert!(f.into_inner().is_nan(), "NaN must round-trip");
            } else {
                assert_eq!(f.into_inner(), special);
            }
        }
    }

    #[test]
    fn relation_element_roundtrips() {
        let change = SourceChange::Update {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("src-1", "rel:1"),
                    labels: Arc::from(vec![Arc::from("OWNS")]),
                    effective_from: 42,
                },
                in_node: ElementReference::new("src-1", "a"),
                out_node: ElementReference::new("src-1", "b"),
                properties: rich_properties(),
            },
        };
        assert_eq!(roundtrip(&change), change);
    }

    #[test]
    fn delete_metadata_roundtrips() {
        let change = SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("src-1", "vehicles:B5678"),
                labels: Arc::from(vec![Arc::from("vehicles")]),
                effective_from: 7,
            },
        };
        assert_eq!(roundtrip(&change), change);
    }

    #[test]
    fn future_ref_roundtrips() {
        let change = SourceChange::Future {
            future_ref: FutureElementRef {
                element_ref: ElementReference::new("src-1", "vehicles:C9999"),
                original_time: 100,
                due_time: 200,
                group_signature: 12345,
            },
        };
        assert_eq!(roundtrip(&change), change);
    }
}
