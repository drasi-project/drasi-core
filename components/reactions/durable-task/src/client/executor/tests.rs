// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

#[derive(Default)]
struct Api {
    state: Option<proto::OrchestrationState>,
    starts: Vec<proto::CreateInstanceRequest>,
    effects: usize,
    lose_start_response: bool,
    disappear_on_effect: bool,
}

impl Api {
    fn effect(&mut self) -> Result<()> {
        self.effects += 1;
        if self.disappear_on_effect {
            return Err(tonic::Status::not_found("target disappeared").into());
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl DtsApi for Api {
    async fn start(
        &mut self,
        request: proto::CreateInstanceRequest,
    ) -> Result<proto::CreateInstanceResponse> {
        self.starts.push(request.clone());
        if self.state.is_some() {
            return Err(tonic::Status::already_exists("existing execution").into());
        }
        self.state = Some(proto::OrchestrationState {
            name: request.name,
            version: request.version,
            input: request.input,
            tags: request.tags,
            ..Default::default()
        });
        if self.lose_start_response {
            return Err(tonic::Status::unavailable("accepted, response lost").into());
        }
        Ok(proto::CreateInstanceResponse {
            instance_id: request.instance_id,
        })
    }

    async fn get(&mut self, _: &str) -> Result<Option<proto::OrchestrationState>> {
        Ok(self.state.clone())
    }

    async fn raise_event(&mut self, _: proto::RaiseEventRequest) -> Result<()> {
        self.effect()
    }

    async fn terminate(&mut self, _: proto::TerminateRequest) -> Result<()> {
        self.effect()
    }
}

fn client(api: Api) -> DtsClient<Api> {
    DtsClient::with_api(
        &DurableTaskReactionConfig {
            max_retries: 1,
            retry_delay_ms: 1,
            ..Default::default()
        },
        api,
    )
}

#[tokio::test(start_paused = true)]
async fn start_reconciliation_and_conflicts_need_no_transport() {
    let mut client = client(Api {
        lose_start_response: true,
        ..Default::default()
    });
    let operation = Dispatch::Start {
        instance_id: "instance".into(),
        orchestration: "Workflow".into(),
        version: "1".into(),
        input: "{\"page\":1}".into(),
        command_id: "command".into(),
        on_conflict: TargetPolicy::Error,
    };
    client.dispatch(&operation).await.unwrap();
    assert_eq!(client.api.starts.len(), 2);
    assert_eq!(client.api.starts[0], client.api.starts[1]);
    let state = client.api.state.as_mut().unwrap();
    state.input = Some("{\"page\":2}".into());
    state.version = Some("2".into());
    client.dispatch(&operation).await.unwrap();
    client.api.state.as_mut().unwrap().tags.clear();
    assert!(client.dispatch(&operation).await.is_err());
    let mut ignored = operation.clone();
    if let Dispatch::Start { on_conflict, .. } = &mut ignored {
        *on_conflict = TargetPolicy::Ignore;
    }
    client.dispatch(&ignored).await.unwrap();
    assert_eq!(
        client.api.state.as_ref().unwrap().input.as_deref(),
        Some("{\"page\":2}")
    );
}

#[tokio::test]
async fn event_target_policies_preserve_no_send_semantics() {
    for (status, on_missing, on_terminal, accepted, effects) in [
        (None, TargetPolicy::Error, TargetPolicy::Ignore, false, 0),
        (None, TargetPolicy::Ignore, TargetPolicy::Error, true, 0),
        (
            Some(proto::OrchestrationStatus::Completed),
            TargetPolicy::Ignore,
            TargetPolicy::Error,
            false,
            0,
        ),
        (
            Some(proto::OrchestrationStatus::Completed),
            TargetPolicy::Error,
            TargetPolicy::Ignore,
            true,
            0,
        ),
        (
            Some(proto::OrchestrationStatus::Running),
            TargetPolicy::Error,
            TargetPolicy::Error,
            true,
            1,
        ),
    ] {
        let mut client = client(Api {
            state: status.map(|status| proto::OrchestrationState {
                orchestration_status: status.into(),
                ..Default::default()
            }),
            ..Default::default()
        });
        let result = client
            .dispatch(&Dispatch::Event {
                instance_id: "target".into(),
                event_name: "event".into(),
                input: "{}".into(),
                on_missing,
                on_terminal,
            })
            .await;
        assert_eq!(result.is_ok(), accepted, "{status:?}: {result:?}");
        assert_eq!(client.api.effects, effects);
    }
}

#[tokio::test]
async fn completed_termination_is_a_noop_and_disappearance_obeys_on_missing() {
    let mut completed = client(Api {
        state: Some(proto::OrchestrationState {
            orchestration_status: proto::OrchestrationStatus::Completed.into(),
            ..Default::default()
        }),
        ..Default::default()
    });
    completed
        .dispatch(&Dispatch::Terminate {
            instance_id: "target".into(),
            output: "{}".into(),
            recursive: true,
            on_missing: TargetPolicy::Error,
        })
        .await
        .unwrap();
    assert_eq!(completed.api.effects, 0);

    for policy in [TargetPolicy::Error, TargetPolicy::Ignore] {
        for operation in [
            Dispatch::Event {
                instance_id: "target".into(),
                event_name: "event".into(),
                input: "{}".into(),
                on_missing: policy,
                on_terminal: TargetPolicy::Error,
            },
            Dispatch::Terminate {
                instance_id: "target".into(),
                output: "{}".into(),
                recursive: false,
                on_missing: policy,
            },
        ] {
            let mut client = client(Api {
                state: Some(proto::OrchestrationState::default()),
                disappear_on_effect: true,
                ..Default::default()
            });
            assert_eq!(
                client.dispatch(&operation).await.is_ok(),
                policy == TargetPolicy::Ignore
            );
            assert_eq!(client.api.effects, 1, "NotFound must not be retried");
        }
    }
}

#[test]
fn jitter_stays_positive_and_within_the_capped_exponential_window() {
    fastrand::seed(17);
    for base in [1, 250, 30_000] {
        for attempt in 0..=10 {
            let cap = (base * (1u64 << attempt)).min(30_000);
            for _ in 0..32 {
                let delay = retry_delay(base, attempt);
                assert!(delay >= Duration::from_millis(cap.div_ceil(2)));
                assert!(delay <= Duration::from_millis(cap));
            }
        }
    }
    let samples: std::collections::HashSet<_> = (0..32).map(|_| retry_delay(250, 0)).collect();
    assert!(
        samples.len() > 1,
        "retry schedules must not stay synchronized"
    );
}
