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

use drasi_core::{
    interface::{FutureElementRef, FutureQueue, PushType, SessionControl},
    models::ElementReference,
};

pub async fn push_always(subject: &impl FutureQueue, session_control: &Arc<dyn SessionControl>) {
    let func1 = 1;
    let group1 = 1;
    let element1 = ElementReference::new("source1", "element1");

    // Push first item (due=20) and commit
    session_control.begin().await.expect("begin failed");
    let push1 = subject
        .push(PushType::Always, func1, group1, &element1, 10, 20)
        .await
        .expect("push failed");
    assert!(push1);
    session_control.commit().await.expect("commit failed");

    // Peek sees committed data
    let peek1 = subject.peek_due_time().await.expect("peek_due_time failed");
    assert_eq!(peek1, Some(20));

    // Push second item (due=30) and commit
    session_control.begin().await.expect("begin failed");
    let push2 = subject
        .push(PushType::Always, func1, group1, &element1, 10, 30)
        .await
        .expect("push failed");
    assert!(push2);
    session_control.commit().await.expect("commit failed");

    // Peek still returns earliest (20)
    let peek2 = subject.peek_due_time().await.expect("peek_due_time failed");
    assert_eq!(peek2, Some(20));

    // Push third item (due=15) and commit
    session_control.begin().await.expect("begin failed");
    let push3 = subject
        .push(PushType::Always, func1, group1, &element1, 10, 15)
        .await
        .expect("push failed");
    assert!(push3);
    session_control.commit().await.expect("commit failed");

    // Peek now returns new earliest (15)
    let peek3 = subject.peek_due_time().await.expect("peek_due_time failed");
    assert_eq!(peek3, Some(15));

    // Pop all items in a single session
    session_control.begin().await.expect("begin failed");

    let pop1 = subject.pop().await.expect("pop failed");
    assert_eq!(
        pop1,
        Some(FutureElementRef {
            element_ref: element1.clone(),
            original_time: 10,
            due_time: 15,
            group_signature: group1,
        })
    );

    let pop2 = subject.pop().await.expect("pop failed");
    assert_eq!(
        pop2,
        Some(FutureElementRef {
            element_ref: element1.clone(),
            original_time: 10,
            due_time: 20,
            group_signature: group1,
        })
    );

    let pop3 = subject.pop().await.expect("pop failed");
    assert_eq!(
        pop3,
        Some(FutureElementRef {
            element_ref: element1.clone(),
            original_time: 10,
            due_time: 30,
            group_signature: group1,
        })
    );

    let pop4 = subject.pop().await.expect("pop failed");
    assert_eq!(pop4, None);

    session_control.commit().await.expect("commit failed");
}

pub async fn push_not_exists(
    subject: &impl FutureQueue,
    session_control: &Arc<dyn SessionControl>,
) {
    let func1 = 1;
    let func2 = 2;
    let group1 = 1;
    let group2 = 2;

    let element1 = ElementReference::new("source1", "element1");

    // Push first item and commit
    session_control.begin().await.expect("begin failed");
    let push1 = subject
        .push(PushType::IfNotExists, func1, group1, &element1, 10, 20)
        .await
        .expect("push failed");
    assert!(push1);
    session_control.commit().await.expect("commit failed");

    // Peek sees committed data
    let peek1 = subject.peek_due_time().await.expect("peek_due_time failed");
    assert_eq!(peek1, Some(20));

    // Push remaining items in a second session
    session_control.begin().await.expect("begin failed");

    // Duplicate (same func+group) — should be rejected
    let push2 = subject
        .push(PushType::IfNotExists, func1, group1, &element1, 10, 30)
        .await
        .expect("push failed");
    assert!(!push2);

    // Different group — should succeed
    let push3 = subject
        .push(PushType::IfNotExists, func1, group2, &element1, 10, 15)
        .await
        .expect("push failed");
    assert!(push3);

    // Different func — should succeed
    let push4 = subject
        .push(PushType::IfNotExists, func2, group2, &element1, 10, 45)
        .await
        .expect("push failed");
    assert!(push4);

    session_control.commit().await.expect("commit failed");

    // Pop all items in a single session
    session_control.begin().await.expect("begin failed");

    let pop1 = subject.pop().await.expect("pop failed");
    assert_eq!(
        pop1,
        Some(FutureElementRef {
            element_ref: element1.clone(),
            original_time: 10,
            due_time: 15,
            group_signature: group2,
        })
    );

    let pop2 = subject.pop().await.expect("pop failed");
    assert_eq!(
        pop2,
        Some(FutureElementRef {
            element_ref: element1.clone(),
            original_time: 10,
            due_time: 20,
            group_signature: group1,
        })
    );

    let pop3 = subject.pop().await.expect("pop failed");
    assert_eq!(
        pop3,
        Some(FutureElementRef {
            element_ref: element1.clone(),
            original_time: 10,
            due_time: 45,
            group_signature: group2,
        })
    );

    let pop4 = subject.pop().await.expect("pop failed");
    assert_eq!(pop4, None);

    session_control.commit().await.expect("commit failed");
}

pub async fn clear_removes_all(
    subject: &impl FutureQueue,
    session_control: &Arc<dyn SessionControl>,
) {
    let element1 = ElementReference::new("source1", "element1");

    session_control.begin().await.expect("begin failed");

    // Push items to populate both the main sorted set and secondary index keys
    subject
        .push(PushType::Always, 1, 1, &element1, 10, 20)
        .await
        .expect("push failed");
    subject
        .push(PushType::Always, 2, 2, &element1, 10, 30)
        .await
        .expect("push failed");

    session_control.commit().await.expect("commit failed");

    // Verify items are present
    let peek = subject.peek_due_time().await.expect("peek failed");
    assert_eq!(peek, Some(20));

    // Clear everything (bypasses sessions — administrative reset)
    subject.clear().await.expect("clear failed");

    // Verify queue is empty
    let peek = subject.peek_due_time().await.expect("peek failed");
    assert_eq!(peek, None);

    session_control.begin().await.expect("begin failed");
    let pop = subject.pop().await.expect("pop failed");
    assert_eq!(pop, None);
    session_control.commit().await.expect("commit failed");
}

pub async fn push_overwrite(
    subject: &impl FutureQueue,
    session_control: &Arc<dyn SessionControl>,
) {
    let func1 = 1;
    let func2 = 2;
    let group1 = 1;
    let group2 = 2;

    let element1 = ElementReference::new("source1", "element1");

    // Push first item (func1, group1, due=20) and commit
    session_control.begin().await.expect("begin failed");
    let push1 = subject
        .push(PushType::Overwrite, func1, group1, &element1, 10, 20)
        .await
        .expect("push failed");
    assert!(push1);
    session_control.commit().await.expect("commit failed");

    // Peek sees committed data
    let peek1 = subject.peek_due_time().await.expect("peek_due_time failed");
    assert_eq!(peek1, Some(20));

    // Overwrite (func1, group1) with due=30 — replaces the due=20 entry
    session_control.begin().await.expect("begin failed");
    let push2 = subject
        .push(PushType::Overwrite, func1, group1, &element1, 10, 30)
        .await
        .expect("push failed");
    assert!(push2);
    session_control.commit().await.expect("commit failed");

    // Peek now returns 30 (20 was overwritten)
    let peek2 = subject.peek_due_time().await.expect("peek_due_time failed");
    assert_eq!(peek2, Some(30));

    // Push remaining items and commit
    session_control.begin().await.expect("begin failed");

    let push3 = subject
        .push(PushType::Overwrite, func1, group2, &element1, 10, 15)
        .await
        .expect("push failed");
    assert!(push3);

    let push4 = subject
        .push(PushType::Overwrite, func2, group2, &element1, 10, 45)
        .await
        .expect("push failed");
    assert!(push4);

    let push5 = subject
        .push(PushType::Overwrite, func2, group2, &element1, 10, 50)
        .await
        .expect("push failed");
    assert!(push5);

    session_control.commit().await.expect("commit failed");

    // Pop all items in a single session
    session_control.begin().await.expect("begin failed");

    let pop1 = subject.pop().await.expect("pop failed");
    assert_eq!(
        pop1,
        Some(FutureElementRef {
            element_ref: element1.clone(),
            original_time: 10,
            due_time: 15,
            group_signature: group2,
        })
    );

    let pop2 = subject.pop().await.expect("pop failed");
    assert_eq!(
        pop2,
        Some(FutureElementRef {
            element_ref: element1.clone(),
            original_time: 10,
            due_time: 30,
            group_signature: group1,
        })
    );

    let pop3 = subject.pop().await.expect("pop failed");
    assert_eq!(
        pop3,
        Some(FutureElementRef {
            element_ref: element1.clone(),
            original_time: 10,
            due_time: 50,
            group_signature: group2,
        })
    );

    let pop4 = subject.pop().await.expect("pop failed");
    assert_eq!(pop4, None);

    session_control.commit().await.expect("commit failed");
}
