// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at http://www.apache.org/licenses/LICENSE-2.0
// Unless required by applicable law or agreed to in writing, software distributed
// under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
// CONDITIONS OF ANY KIND, either express or implied. See the License for the
// specific language governing permissions and limitations under the License.

#[allow(dead_code)]
mod computation_support;
#[path = "computation_support/pipe_profiles.rs"]
mod profiles;

use std::{collections::BTreeSet, num::NonZeroUsize, sync::Arc, time::Duration};

use computation_support::*;
use drasi_lib::computation::v1::*;
use profiles::{timed_event, Profile};

const CAPABILITIES: [PipeCapability; 9] = [
    PipeCapability::FifoPerStream,
    PipeCapability::RankedEventOrder,
    PipeCapability::Backpressure,
    PipeCapability::DurableAcceptance,
    PipeCapability::ExplicitAcknowledgement,
    PipeCapability::Replay,
    PipeCapability::RetainedHistory,
    PipeCapability::Transactions,
    PipeCapability::ExactlyOnce,
];

fn capability_set(mask: usize) -> BTreeSet<PipeCapability> {
    CAPABILITIES
        .into_iter()
        .enumerate()
        .filter_map(|(bit, capability)| (mask & (1 << bit) != 0).then_some(capability))
        .collect()
}

#[test]
fn every_capability_declaration_and_requirement_combination_is_negotiated() {
    use PipeCapability::*;
    for mask in 0..512 {
        let supported = capability_set(mask);
        let has = |capability| supported.contains(&capability);
        let valid = (!has(Replay) || has(DurableAcceptance))
            && (!has(Transactions) || has(ExplicitAcknowledgement))
            && (!has(ExactlyOnce)
                || (has(DurableAcceptance) && has(ExplicitAcknowledgement) && has(Transactions)));
        for capacity in [None, NonZeroUsize::new(1), NonZeroUsize::new(usize::MAX)] {
            let declaration = PipeCapabilities::try_new(supported.clone(), capacity);
            assert_eq!(declaration.is_ok(), valid, "{supported:?}");
            let Ok(declaration) = declaration else {
                continue;
            };
            assert_eq!(declaration.capacity(), capacity);
            assert_eq!(declaration.supported(), &supported);
            for requirements in 0..512 {
                let required = capability_set(requirements);
                let requirements = PipeRequirements::new(required.clone());
                assert_eq!(requirements.required(), &required);
                let missing = required.difference(&supported).next();
                match (declaration.validate(&requirements), missing) {
                    (Ok(()), None) => {}
                    (Err(ContractError::UnsupportedCapability { capability }), Some(expected)) => {
                        assert_eq!(&capability, expected);
                    }
                    (actual, expected) => panic!("{supported:?}: {actual:?}, missing {expected:?}"),
                }
                for demanding_output in [false, true] {
                    let endpoint = |direction, demanding| {
                        PortDescriptor::new(
                            port(if direction == PortDirection::Input {
                                "in"
                            } else {
                                "out"
                            }),
                            direction,
                            schema_descriptor(),
                            if demanding {
                                requirements.clone()
                            } else {
                                PipeRequirements::default()
                            },
                        )
                    };
                    let output = endpoint(PortDirection::Output, demanding_output);
                    let input = endpoint(PortDirection::Input, !demanding_output);
                    assert_eq!(
                        validate_connection(&output, &input, &declaration).is_ok(),
                        missing.is_none() && (has(FifoPerStream) || has(RankedEventOrder)),
                        "output={demanding_output}, supported={supported:?}, required={required:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn every_sink_completion_requirement_combination_is_checked() {
    for mask in 0..512 {
        let required = capability_set(mask);
        let handling_required = [
            PipeCapability::ExplicitAcknowledgement,
            PipeCapability::Transactions,
            PipeCapability::ExactlyOnce,
        ]
        .into_iter()
        .any(|capability| required.contains(&capability));
        let requirements = PipeRequirements::new(required);
        assert!(validate_sink_completion(SinkCompletion::Handled, &requirements).is_ok());
        assert_eq!(
            validate_sink_completion(SinkCompletion::Accepted, &requirements).is_ok(),
            !handling_required
        );
    }
}

#[test]
fn descriptor_roundtrips_revalidate_all_schema_and_component_fields() {
    let original = descriptor("source", &[], &["out"])
        .with_plugin_identity(PluginIdentity {
            id: Arc::from("test/plugin"),
            version: Arc::from("1"),
        })
        .expect("identity")
        .with_semantic_kind(ComponentSemanticKind::Source);
    let value = serde_json::to_value(&original).expect("serialized descriptor");
    let restored: ComponentDescriptor = serde_json::from_value(value.clone()).expect("roundtrip");
    assert_eq!(restored, original);
    assert_eq!(
        restored.semantic_kind(),
        Some(ComponentSemanticKind::Source)
    );
    assert_eq!(
        restored.plugin_identity().expect("plugin").version.as_ref(),
        "1"
    );
    for (field, invalid) in [
        ("id", serde_json::json!("")),
        ("version", serde_json::json!(0)),
        ("encoding", serde_json::json!("invalid encoding")),
        ("definition", serde_json::json!([])),
    ] {
        let mut changed = value.clone();
        changed["ports"][0]["schema"][field] = invalid;
        assert!(
            serde_json::from_value::<ComponentDescriptor>(changed).is_err(),
            "{field}"
        );
    }
    for field in ["id", "version"] {
        for invalid in ["", "with space", "with\ncontrol"] {
            let mut changed = value.clone();
            changed["plugin_identity"][field] = serde_json::json!(invalid);
            assert!(serde_json::from_value::<ComponentDescriptor>(changed).is_err());
        }
    }
    let mut duplicate = value.clone();
    duplicate["ports"] = serde_json::json!([value["ports"][0], value["ports"][0]]);
    assert!(serde_json::from_value::<ComponentDescriptor>(duplicate).is_err());
    let error = RecordValidationError::new("bad-record", "identity does not match payload");
    assert_eq!(error.code(), "bad-record");
    assert_eq!(error.message(), "identity does not match payload");
}

#[test]
fn all_port_direction_pairs_and_schema_dimensions_are_checked() {
    let capabilities = PipeCapabilities::volatile_bounded(NonZeroUsize::new(1).expect("capacity"));
    for output in [PortDirection::Input, PortDirection::Output] {
        for input in [PortDirection::Input, PortDirection::Output] {
            let descriptor = |direction| {
                PortDescriptor::new(
                    port("p"),
                    direction,
                    schema_descriptor(),
                    PipeRequirements::default(),
                )
            };
            assert_eq!(
                validate_connection(&descriptor(output), &descriptor(input), &capabilities).is_ok(),
                output == PortDirection::Output && input == PortDirection::Input
            );
        }
    }
    let schema = schema_descriptor();
    for changed in [
        named_schema("different"),
        SchemaDescriptor::try_new(
            schema.id().clone(),
            SchemaVersion::try_new(2).expect("version"),
            schema.encoding(),
            schema.definition().clone(),
        )
        .expect("version variant"),
        SchemaDescriptor::try_new(
            schema.id().clone(),
            schema.version(),
            "different",
            schema.definition().clone(),
        )
        .expect("encoding variant"),
        SchemaDescriptor::try_new(
            schema.id().clone(),
            schema.version(),
            schema.encoding(),
            bytes::Bytes::from_static(b"different"),
        )
        .expect("definition variant"),
    ] {
        let output = PortDescriptor::new(
            port("out"),
            PortDirection::Output,
            schema.clone(),
            PipeRequirements::default(),
        );
        let input = PortDescriptor::new(
            port("in"),
            PortDirection::Input,
            changed.clone(),
            PipeRequirements::default(),
        );
        match validate_connection(&output, &input, &capabilities) {
            Err(ContractError::SchemaMismatch { expected, actual }) => {
                assert_eq!(*expected, schema);
                assert_eq!(*actual, changed);
            }
            other => panic!("expected exact descriptor mismatch, got {other:?}"),
        }
    }
}

async fn finish(delivery: Delivery, expected: &ChangeEnvelope, acknowledgement: bool) {
    let (envelope, ack) = delivery.into_parts();
    assert!(Arc::ptr_eq(envelope.event(), expected.event()));
    assert_eq!(ack.is_some(), acknowledgement);
    if let Some(ack) = ack {
        ack.complete(HandlingOutcome::Handled)
            .await
            .expect("handling");
    }
}

#[tokio::test]
async fn every_pipe_profile_honors_ownership_cancellation_and_graceful_drain() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for profile in Profile::ALL {
            let (provider, resources) = profile.provider(
                "queue",
                stream("source"),
                NonZeroUsize::new(2).expect("capacity"),
            );
            provider.validate_resources(&resources).expect("resources");
            assert!(provider.specification().is_some());
            let advertised = provider.capabilities().expect("capabilities");
            assert_eq!(
                advertised
                    .supported()
                    .contains(&PipeCapability::Backpressure),
                profile.backpressure()
            );
            assert_eq!(
                advertised
                    .supported()
                    .contains(&PipeCapability::ExplicitAcknowledgement),
                profile.acknowledgement()
            );
            for unsupported in [
                PipeCapability::DurableAcceptance,
                PipeCapability::Replay,
                PipeCapability::Transactions,
                PipeCapability::ExactlyOnce,
            ] {
                assert!(
                    !advertised.supported().contains(&unsupported),
                    "{profile:?}"
                );
            }
            let mut supplied = provider.create_with_resources(&resources).expect("pipe");
            assert_eq!(supplied.pipe.capabilities(), &advertised);
            assert!(supplied.control.is_idle().await.expect("initially idle"));
            let _ = supplied.pipe.metrics();
            let sender = supplied.pipe.sender();
            let mut receiver = supplied.pipe.take_receiver().expect("receiver");
            assert!(matches!(
                supplied.pipe.take_receiver(),
                Err(PipeError::ReceiverTaken)
            ));
            drop(supplied.pipe);
            let mut cancelled_receive = Box::pin(receiver.receive());
            assert!(
                futures::poll!(&mut cancelled_receive).is_pending(),
                "{profile:?}"
            );
            drop(cancelled_receive);
            let events = [timed_event(1), timed_event(2)];
            let receipts = sender.send_batch(events.to_vec()).await.expect("batch");
            assert_eq!(
                receipts
                    .iter()
                    .map(EnqueueReceipt::envelope_id)
                    .collect::<Vec<_>>(),
                events.iter().map(ChangeEnvelope::id).collect::<Vec<_>>()
            );
            assert!(!supplied.control.is_idle().await.expect("not yet delivered"));
            supplied.control.close();
            supplied.control.close();
            let rejected = timed_event(3);
            let failed = sender.send(rejected.clone()).await.expect_err("closed");
            assert_eq!(failed.acceptance(), AcceptanceState::NotAccepted);
            assert!(Arc::ptr_eq(failed.envelope.event(), rejected.event()));
            for event in &events {
                finish(
                    receiver
                        .receive()
                        .await
                        .expect("receive")
                        .expect("accepted event"),
                    event,
                    profile.acknowledgement(),
                )
                .await;
            }
            assert!(supplied.control.is_idle().await.expect("drained"));
            assert!(receiver.receive().await.expect("end").is_none());
            supplied.control.cancel();
            supplied.control.close();
            assert!(matches!(
                supplied.control.is_idle().await,
                Err(PipeError::Closed)
            ));
            if matches!(profile, Profile::Qos | Profile::QosLossy) {
                assert!(matches!(receiver.receive().await, Err(PipeError::Closed)));
            } else {
                assert!(receiver.receive().await.expect("cancelled").is_none());
            }
        }
    })
    .await
    .expect("pipe lifecycle must not hang");
}

#[tokio::test]
async fn lossless_profiles_release_capacity_only_at_their_declared_boundary() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for profile in Profile::ALL
            .into_iter()
            .filter(|profile| profile.backpressure())
        {
            let (provider, resources) = profile.provider(
                "queue",
                stream("source"),
                NonZeroUsize::new(1).expect("capacity"),
            );
            let mut supplied = provider.create_with_resources(&resources).expect("pipe");
            let sender = supplied.pipe.sender();
            let mut receiver = supplied.pipe.take_receiver().expect("receiver");
            sender.send(timed_event(1)).await.expect("first");
            let mut abandoned = Box::pin(sender.send(timed_event(99)));
            assert!(futures::poll!(&mut abandoned).is_pending());
            drop(abandoned);
            let second = timed_event(2);
            let mut pending = Box::pin(sender.send(second.clone()));
            assert!(futures::poll!(&mut pending).is_pending(), "{profile:?}");
            let first = receiver.receive().await.expect("receive").expect("first");
            if profile.acknowledgement() {
                assert!(
                    futures::poll!(&mut pending).is_pending(),
                    "delivery is not handling"
                );
                let (_, ack) = first.into_parts();
                ack.expect("explicit acknowledgement")
                    .complete(HandlingOutcome::Failed {
                        reason: "retry this event".into(),
                    })
                    .await
                    .expect("failure recorded");
                assert!(futures::poll!(&mut pending).is_pending());
                let replay = receiver
                    .receive()
                    .await
                    .expect("replay")
                    .expect("unhandled");
                assert_eq!(replay.envelope().system().sequence(), 1);
                replay
                    .into_parts()
                    .1
                    .expect("explicit acknowledgement")
                    .complete(HandlingOutcome::Handled)
                    .await
                    .expect("handled");
            } else {
                assert!(first.into_parts().1.is_none());
            }
            assert_eq!(
                pending.await.expect("capacity freed").envelope_id(),
                second.id()
            );
            finish(
                receiver.receive().await.expect("receive").expect("second"),
                &second,
                profile.acknowledgement(),
            )
            .await;
            assert!(supplied.control.is_idle().await.expect("idle"));
            sender.send(timed_event(3)).await.expect("fill again");
            let original = timed_event(4);
            let mut blocked = Box::pin(sender.send(original.clone()));
            assert!(futures::poll!(&mut blocked).is_pending());
            drop(receiver);
            let failure = blocked.await.expect_err("receiver dropped");
            assert_eq!(failure.acceptance(), AcceptanceState::NotAccepted);
            assert!(matches!(failure.error, PipeError::Closed));
            assert!(Arc::ptr_eq(failure.envelope.event(), original.event()));
        }
    })
    .await
    .expect("blocked senders must be woken");
}

#[tokio::test]
async fn lossy_profiles_report_the_exact_gap_and_keep_the_documented_end_of_the_queue() {
    for profile in [
        Profile::Broadcast,
        Profile::RankedLossy,
        Profile::RetainedLossy,
        Profile::QosLossy,
    ] {
        let (provider, resources) = profile.provider(
            "queue",
            stream("source"),
            NonZeroUsize::new(2).expect("capacity"),
        );
        let mut supplied = provider.create_with_resources(&resources).expect("pipe");
        let sender = supplied.pipe.sender();
        let mut receiver = supplied.pipe.take_receiver().expect("receiver");
        for sequence in 1..=5 {
            sender
                .send(timed_event(sequence))
                .await
                .expect("lossy acceptance");
        }
        match profile {
            Profile::Broadcast => assert!(matches!(
                receiver.receive().await,
                Err(PipeError::Lagged { skipped: 3 })
            )),
            Profile::RetainedLossy | Profile::QosLossy => {
                for _ in 0..2 {
                    assert!(
                        matches!(
                            receiver.receive().await,
                            Err(PipeError::PositionUnavailable {
                                requested: 0,
                                oldest: 4
                            })
                        ),
                        "{profile:?}: strict gaps must not advance progress"
                    );
                }
                supplied.control.cancel();
                continue;
            }
            _ => {}
        }
        supplied.control.close();
        let expected = if profile == Profile::RankedLossy {
            [1, 2]
        } else {
            [4, 5]
        };
        for sequence in expected {
            assert_eq!(
                receiver
                    .receive()
                    .await
                    .expect("receive")
                    .expect("retained event")
                    .envelope()
                    .system()
                    .sequence(),
                sequence
            );
        }
        assert!(receiver.receive().await.expect("end").is_none());
        let metrics = supplied.control.metrics().expect("volatile metrics");
        assert_eq!(
            (
                metrics.accepted,
                metrics.delivered,
                metrics.discarded,
                metrics.queued
            ),
            (5, 2, 3, 0)
        );
    }
}

#[tokio::test]
async fn dropping_the_last_sender_wakes_every_profile_without_control_keeping_it_alive() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for profile in Profile::ALL {
            let (provider, resources) = profile.provider(
                "queue",
                stream("source"),
                NonZeroUsize::new(1).expect("capacity"),
            );
            let mut supplied = provider.create_with_resources(&resources).expect("pipe");
            let mut receiver = supplied.pipe.take_receiver().expect("receiver");
            let sender = supplied.pipe.sender();
            drop(supplied.pipe);
            let mut waiting = Box::pin(receiver.receive());
            assert!(futures::poll!(&mut waiting).is_pending());
            drop(sender);
            assert!(waiting.await.expect("closed").is_none(), "{profile:?}");
            assert!(supplied.control.is_idle().await.expect("idle"));
        }
    })
    .await
    .expect("sender-independent control must not prevent end of stream");
}

#[test]
fn providers_reject_invalid_capacity_missing_bindings_wrong_types_and_mismatched_resources() {
    for capacity in [0, tokio::sync::Semaphore::MAX_PERMITS + 1, usize::MAX] {
        assert!(matches!(
            BoundedPipeConfig { capacity }.capabilities(),
            Err(PipeError::InvalidCapacity)
        ));
        assert!(matches!(
            BoundedPipe::new(capacity),
            Err(PipeError::InvalidCapacity)
        ));
    }
    assert!(matches!(
        BroadcastPipe::new(BroadcastPipeConfig {
            capacity: 0,
            lag_policy: BroadcastLagPolicy::Report
        }),
        Err(PipeError::InvalidCapacity)
    ));
    assert!(RankedInputQueue::new(0).is_err());
    for profile in Profile::ALL {
        let (provider, resources) = profile.provider(
            "queue",
            stream("source"),
            NonZeroUsize::new(1).expect("capacity"),
        );
        if resources.is_empty() {
            continue;
        }
        assert!(
            provider.create().is_err(),
            "{profile:?}: resources are mandatory"
        );
        assert!(provider.create_with_resources(&Default::default()).is_err());
        let wrong = std::collections::BTreeMap::from([(
            ResourceId::try_new("queue").expect("resource"),
            ResourceHandle::new(ResourceRole::StateStore, Arc::new(42_u64)),
        )]);
        assert!(provider.validate_resources(&wrong).is_err());
        assert!(provider.create_with_resources(&wrong).is_err());
        let (mismatch, _) = profile.provider(
            "queue",
            stream("source"),
            NonZeroUsize::new(2).expect("capacity"),
        );
        assert!(mismatch.validate_resources(&resources).is_err());
        assert!(mismatch.create_with_resources(&resources).is_err());
    }
}
