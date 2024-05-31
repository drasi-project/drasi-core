use drasi_core::{
    interface::{FutureElementRef, FutureQueue, PushType},
    models::ElementReference,
};

pub async fn push_always(subject: &impl FutureQueue) {
    let func1 = 1;
    let group1 = 1;
    let element1 = ElementReference::new("source1", "element1");

    let push1 = subject
        .push(PushType::Always, func1, group1, &element1, 10, 20)
        .await
        .expect("push failed");
    assert!(push1);

    let peek1 = subject.peek_due_time().await.expect("peek_due_time failed");
    assert_eq!(peek1, Some(20));

    let push2 = subject
        .push(PushType::Always, func1, group1, &element1, 10, 30)
        .await
        .expect("push failed");
    assert!(push2);

    let peek2 = subject.peek_due_time().await.expect("peek_due_time failed");
    assert_eq!(peek2, Some(20));

    let push3 = subject
        .push(PushType::Always, func1, group1, &element1, 10, 15)
        .await
        .expect("push failed");
    assert!(push3);

    let peek3 = subject.peek_due_time().await.expect("peek_due_time failed");
    assert_eq!(peek3, Some(15));

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
}

pub async fn push_not_exists(subject: &impl FutureQueue) {
    let func1 = 1;
    let func2 = 2;
    let group1 = 1;
    let group2 = 2;

    let element1 = ElementReference::new("source1", "element1");

    let push1 = subject
        .push(PushType::IfNotExists, func1, group1, &element1, 10, 20)
        .await
        .expect("push failed");
    assert!(push1);

    let peek1 = subject.peek_due_time().await.expect("peek_due_time failed");
    assert_eq!(peek1, Some(20));

    let push2 = subject
        .push(PushType::IfNotExists, func1, group1, &element1, 10, 30)
        .await
        .expect("push failed");
    assert_eq!(push2, false);

    let push3 = subject
        .push(PushType::IfNotExists, func1, group2, &element1, 10, 15)
        .await
        .expect("push failed");
    assert!(push3);

    let push4 = subject
        .push(PushType::IfNotExists, func2, group2, &element1, 10, 45)
        .await
        .expect("push failed");
    assert!(push4);

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
}

pub async fn push_overwrite(subject: &impl FutureQueue) {
    let func1 = 1;
    let func2 = 2;
    let group1 = 1;
    let group2 = 2;

    let element1 = ElementReference::new("source1", "element1");

    let push1 = subject
        .push(PushType::Overwrite, func1, group1, &element1, 10, 20)
        .await
        .expect("push failed");
    assert!(push1);

    let peek1 = subject.peek_due_time().await.expect("peek_due_time failed");
    assert_eq!(peek1, Some(20));

    let push2 = subject
        .push(PushType::Overwrite, func1, group1, &element1, 10, 30)
        .await
        .expect("push failed");
    assert!(push2);

    let peek2 = subject.peek_due_time().await.expect("peek_due_time failed");
    assert_eq!(peek2, Some(30));

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
}
