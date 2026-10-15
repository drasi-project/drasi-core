// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_source_grpc::proto::{admission, source};
use prost::Message;

#[test]
fn admission_uses_the_shared_source_message_without_changing_encoding() {
    let event = drasi_grpc_common::source::SourceChange {
        r#type: source::ChangeType::Insert as i32,
        source_id: "s".into(),
        ..Default::default()
    };
    let request = admission::AdmitRequest {
        source_id: "s".into(),
        session: Some(admission::ProducerSession {
            incarnation: "i".into(),
            producer: "p".into(),
            epoch: 3,
        }),
        sequence: 7,
        event: Some(event),
    };
    assert_eq!(
        request.encode_to_vec(),
        [10, 1, b's', 18, 8, 10, 1, b'i', 18, 1, b'p', 24, 3, 24, 7, 34, 5, 8, 1, 42, 1, b's']
    );
}
