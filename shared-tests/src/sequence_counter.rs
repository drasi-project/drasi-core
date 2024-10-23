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

use std::sync::Arc;

use drasi_core::interface::{ResultSequence, ResultSequenceCounter};

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
