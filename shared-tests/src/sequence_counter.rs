use std::sync::Arc;

use drasi_query_core::interface::{ResultSequence, ResultSequenceCounter};

pub async fn sequence_counter(subject: &dyn ResultSequenceCounter) {
    let result = subject.get_sequence().await.expect("get_sequence failed");
    assert_eq!(result, ResultSequence::default());

    subject
        .apply_sequence(2, "foo")
        .await
        .expect("apply_sequence failed");

    let result = subject.get_sequence().await.expect("get_sequence failed");
    assert_eq!(
        result,
        ResultSequence {
            sequence: 2,
            source_change_id: Arc::from("foo"),
        }
    );
}
