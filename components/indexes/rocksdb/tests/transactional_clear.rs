// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;

use drasi_core::{
    evaluation::functions::aggregation::ValueAccumulator,
    interface::{CreatedIndexes, IndexBackendPlugin, PushType, ResultKey, ResultOwner},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use ordered_float::OrderedFloat;

async fn clear(indexes: &CreatedIndexes) {
    indexes
        .set
        .element_index
        .clear()
        .await
        .expect("clear elements");
    indexes
        .set
        .archive_index
        .clear()
        .await
        .expect("clear archive");
    indexes
        .set
        .result_index
        .clear()
        .await
        .expect("clear accumulators");
    indexes
        .set
        .future_queue
        .clear()
        .await
        .expect("clear futures");
}

async fn assert_state(indexes: &CreatedIndexes, reference: &ElementReference, present: bool) {
    assert_eq!(
        indexes
            .set
            .element_index
            .get_element(reference)
            .await
            .expect("read element")
            .is_some(),
        present
    );
    assert_eq!(
        indexes
            .set
            .archive_index
            .get_element_as_at(reference, 10)
            .await
            .expect("read archived element")
            .is_some(),
        present
    );
    let sum = indexes
        .set
        .result_index
        .get(&ResultKey::InputHash(1), &ResultOwner::Function(1))
        .await
        .expect("read accumulator");
    assert_eq!(sum.is_some(), present);
    assert_eq!(
        indexes
            .set
            .result_index
            .get_value_count(17, OrderedFloat(2.5))
            .await
            .expect("read lazy set"),
        isize::from(present)
    );
    assert_eq!(
        indexes
            .set
            .future_queue
            .pop()
            .await
            .expect("read transactional future")
            .is_some(),
        present
    );
    assert_eq!(
        indexes
            .set
            .result_index
            .get_sequence()
            .await
            .expect("read result sequence")
            .sequence,
        77
    );
}

#[tokio::test]
async fn clearing_indexes_joins_the_transaction_and_preserves_column_families() {
    let directory = tempfile::tempdir().unwrap();
    let provider = RocksDbIndexProvider::new(directory.path(), true, false);
    let indexes = provider.create_indexes("clear").await.unwrap();
    let reference = ElementReference::new("source", "item");
    let node = Element::Node {
        metadata: ElementMetadata {
            reference: reference.clone(),
            labels: Arc::from([Arc::from("Item")]),
            effective_from: 10,
        },
        properties: ElementPropertyMap::from(serde_json::json!({"value":7})),
    };
    indexes.set.session_control.begin().await.unwrap();
    indexes
        .set
        .element_index
        .set_element(&node, &vec![0])
        .await
        .unwrap();
    indexes
        .set
        .result_index
        .set(
            ResultKey::InputHash(1),
            ResultOwner::Function(1),
            Some(ValueAccumulator::Sum { value: 7.0 }),
        )
        .await
        .unwrap();
    indexes
        .set
        .result_index
        .increment_value_count(17, OrderedFloat(2.5), 1)
        .await
        .unwrap();
    indexes
        .set
        .result_index
        .apply_sequence(77, "source")
        .await
        .unwrap();
    indexes
        .set
        .future_queue
        .push(PushType::Always, 1, 42, &reference, 10, 20)
        .await
        .unwrap();
    indexes.set.session_control.commit().await.unwrap();
    let options_files = || {
        let mut files: Vec<_> = std::fs::read_dir(directory.path().join("clear"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with("OPTIONS-"))
            .collect();
        files.sort();
        files
    };
    let options = options_files();
    indexes.set.session_control.begin().await.unwrap();
    clear(&indexes).await;
    assert_state(&indexes, &reference, false).await;
    indexes.set.session_control.rollback().unwrap();
    indexes.set.session_control.begin().await.unwrap();
    assert_state(&indexes, &reference, true).await;
    clear(&indexes).await;
    indexes.set.session_control.commit().await.unwrap();
    assert_eq!(
        options_files(),
        options,
        "data clearing must not rewrite the CF OPTIONS files"
    );
    drop(indexes);
    let reopened = provider.create_indexes("clear").await.unwrap();
    reopened.set.session_control.begin().await.unwrap();
    assert_state(&reopened, &reference, false).await;
    reopened.set.session_control.commit().await.unwrap();
}
