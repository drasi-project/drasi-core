// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[cfg(any(feature = "source", feature = "reaction"))]
use prost::Message;

#[cfg(feature = "source")]
#[test]
fn source_messages_preserve_field_numbers_and_event_encoding() {
    let reference = crate::source::ElementReference {
        source_id: "s".into(),
        element_id: "n".into(),
    };
    assert_eq!(reference.encode_to_vec(), [10, 1, b's', 18, 1, b'n']);
    let event = crate::source::SourceChange {
        r#type: crate::source::ChangeType::Insert as i32,
        source_id: "s".into(),
        ..Default::default()
    };
    assert_eq!(event.encode_to_vec(), [8, 1, 42, 1, b's']);
}

#[cfg(feature = "reaction")]
#[test]
fn reaction_items_preserve_operation_identity_and_sequence_encoding() {
    let item = crate::reaction::QueryResultItem {
        item_type: crate::reaction::QueryResultItemType::Add as i32,
        row_signature: 42,
        sequence: 7,
        ..Default::default()
    };
    assert_eq!(item.encode_to_vec(), [8, 1, 16, 42, 40, 7]);
    assert_eq!(
        crate::reaction::QueryResultItem::decode(item.encode_to_vec().as_slice())
            .expect("known reaction encoding"),
        item
    );
}

#[cfg(all(feature = "source", feature = "reaction"))]
#[test]
fn historical_result_contracts_remain_distinct() {
    let source_item = crate::source::QueryResultItem {
        r#type: "ADD".into(),
        ..Default::default()
    };
    let reaction_item = crate::reaction::QueryResultItem {
        item_type: crate::reaction::QueryResultItemType::Add as i32,
        ..Default::default()
    };
    assert_eq!(source_item.encode_to_vec(), [10, 3, b'A', b'D', b'D']);
    assert_eq!(reaction_item.encode_to_vec(), [8, 1]);
}
