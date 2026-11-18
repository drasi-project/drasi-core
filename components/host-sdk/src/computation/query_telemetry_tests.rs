// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_sdk as sdk;
use drasi_computation_plugin_sdk::{
    query_telemetry::QueryTelemetry, query_transport::QueryBeginReply,
};
use drasi_lib::computation::v1::*;
use std::task::Poll;

unsafe extern "C" fn unused(
    _: *mut c_void,
    _: u32,
    _: abi::BorrowedBytes,
    _: *const abi::Transaction,
    _: *mut abi::OperationHandle,
) -> abi::Status {
    abi::Status::ok()
}

unsafe extern "C" fn supports_factory(_: *mut c_void, out: *mut u32) -> abi::Status {
    transport::status_boundary(|| unsafe { transport::write_out(out, 1) })
}

fn extensions() -> (PluginTelemetryV2, PluginTelemetryV4) {
    (
        PluginTelemetryV2 {
            header: abi::Header::new::<PluginTelemetryV2>(),
            version: VERSION_V2,
            capabilities: ALL,
            reserved: 0,
            begin: Some(unused),
            begin_consumer: Some(unused),
            with_scope: Some(drasi_computation_plugin_sdk::telemetry_bridge::with_scope),
        },
        PluginTelemetryV4 {
            header: abi::Header::new::<PluginTelemetryV4>(),
            version: VERSION_V4,
            capabilities: CONTEXT | QUERY_OBSERVATIONS,
            reserved: 0,
            supports_factory: Some(supports_factory),
            with_scope: Some(drasi_computation_plugin_sdk::telemetry_bridge::with_query_scope),
        },
    )
}

fn setup(required: bool) -> anyhow::Result<(ComputationGraph, ExecutionScope)> {
    let mut settings = TelemetrySettings::new(TelemetryProfile::Basic).with_tracing(false);
    if required {
        settings = settings.require_native_query_observations();
    }
    setup_settings(settings)
}

fn setup_settings(
    settings: TelemetrySettings,
) -> anyhow::Result<(ComputationGraph, ExecutionScope)> {
    let graph = ComputationGraph::builder("query-transport")
        .with_telemetry(
            TelemetryOptions::new(settings)
                .with_instance_id("query-transport")
                .with_metric_context(MetricRegistrationContext::new(
                    Arc::new(metrics::NoopRecorder),
                    Default::default(),
                )?),
        )
        .build()?;
    let binding = graph.telemetry().plugin_binding(
        TelemetryScope::graph("query-transport", "query-transport")?.component(
            ComponentId::try_new("d5bd71eb-52ae-4abb-a435-f707c5376509")?,
            ComponentRole::Transformer,
            ComponentSemanticKind::Query,
        ),
        7,
        "query",
        None,
    )?;
    let scope = ExecutionScope::new(Default::default())
        .with_component(binding.config().scope.clone(), 7)
        .with_plugin_telemetry(Some(binding));
    Ok((graph, scope))
}

#[test]
fn sdk_query_enabled_uses_host_policy_not_downgraded_v2_trace_flags() -> anyhow::Result<()> {
    let (v2, v4) = extensions();
    for (settings, enabled) in [
        (TelemetrySettings::new(TelemetryProfile::Off), false),
        (
            TelemetrySettings::new(TelemetryProfile::Basic)
                .with_tracing(false)
                .exclude_metrics("drasi.query."),
            false,
        ),
        (
            TelemetrySettings::new(TelemetryProfile::Debug)
                .with_metrics(false)
                .require_native_query_observations(),
            true,
        ),
    ] {
        let (_graph, scope) = setup_settings(settings)?;
        let binding = scope
            .in_scope(|| NativeTelemetryBinding::capture_query(Some(v2), Some(v4), "query"))?
            .expect("context binding");
        assert!(
            !binding.config.tracing,
            "no native span collector is installed"
        );
        assert_eq!(binding.query_observations_active(), enabled);
        let query = binding.scope(|| Ok(QueryTelemetry::capture()?))?;
        assert_eq!(query.enabled(), enabled);
    }
    Ok(())
}

fn begin(binding: &NativeTelemetryBinding) -> anyhow::Result<QueryToken> {
    let frame = encode_telemetry(
        &QueryObservationRequest {
            kind: QueryObservationKind::Transaction,
            phase: QueryObservationPhase::Steady,
            parent: None,
            change_count: Some(1),
        },
        MAX_TELEMETRY_RECORD_BYTES,
    )?;
    let reply = unsafe {
        transport::take_reply(begin_query(
            binding.table().context,
            abi::BorrowedBytes::new(&frame),
        ))?
    };
    let reply: Option<QueryBeginReply> = wire::decode(&reply)?;
    let reply = reply.expect("query observation enabled");
    Ok(QueryToken {
        owner: reply.owner,
        lease: reply.lease,
    })
}

#[test]
fn query_tokens_reject_foreign_replay_and_release_capacity() -> anyhow::Result<()> {
    let (_graph, scope) = setup(true)?;
    let (v2, v4) = extensions();
    let binding = scope
        .in_scope(|| NativeTelemetryBinding::capture_query(Some(v2), Some(v4), "query"))?
        .expect("binding");
    let token = begin(&binding)?;
    let mut foreign = token;
    foreign.owner[0] ^= 1;
    let frame = encode_telemetry(
        &QueryObservationResult {
            outcome: QueryObservationOutcome::Success,
            result_count: Some(0),
        },
        MAX_TELEMETRY_RECORD_BYTES,
    )?;
    let finish = |token| unsafe {
        transport::take_status(finish_query(
            binding.table().context,
            token,
            abi::BorrowedBytes::new(&frame),
        ))
    };
    assert!(finish(foreign).is_err());
    assert_eq!(binding.state.queries.lock().unwrap().entries.len(), 1);
    finish(token)?;
    assert!(finish(token).is_err());
    assert!(binding.state.queries.lock().unwrap().entries.is_empty());
    let mut tokens = Vec::new();
    for _ in 0..binding.config.limits.open_spans {
        tokens.push(begin(&binding)?);
    }
    assert!(begin(&binding).is_err());
    finish(tokens.pop().unwrap())?;
    let replacement = begin(&binding)?;
    assert_ne!(replacement.lease, token.lease);
    drop(binding);
    Ok(())
}

#[tokio::test]
async fn sdk_guard_restores_host_scope_per_poll_and_cancels_once() -> anyhow::Result<()> {
    let (_graph, scope) = setup(true)?;
    let (v2, v4) = extensions();
    let binding = scope
        .in_scope(|| NativeTelemetryBinding::capture_query(Some(v2), Some(v4), "query"))?
        .expect("binding");
    let query = binding.scope(|| Ok(QueryTelemetry::capture()?))?;
    assert!(query.available());
    let mut guard = query
        .begin(
            QueryObservationKind::Transaction,
            QueryObservationPhase::Steady,
            None,
        )?
        .expect("transaction guard");
    let mut polls = 0;
    guard
        .scope(std::future::poll_fn(|cx| {
            assert!(NativeQueryScope::current().is_some(), "host scope restored");
            assert!(
                QueryTelemetry::capture().unwrap().enabled(),
                "plugin binding restored"
            );
            polls += 1;
            if polls == 1 {
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(Ok::<_, anyhow::Error>(()))
            }
        }))
        .await?;
    assert_eq!(polls, 2);
    assert!(NativeQueryScope::current().is_none());
    guard.finish(QueryObservationOutcome::Success, None)?;
    guard.finish(QueryObservationOutcome::Success, None)?;
    assert!(binding.state.queries.lock().unwrap().entries.is_empty());
    let unfinished = query
        .begin(
            QueryObservationKind::Evaluate,
            QueryObservationPhase::Steady,
            Some(1),
        )?
        .expect("evaluation guard");
    drop(unfinished);
    assert!(binding.state.queries.lock().unwrap().entries.is_empty());
    struct PendingDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl std::future::Future for PendingDrop {
        type Output = anyhow::Result<()>;
        fn poll(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> Poll<Self::Output> {
            Poll::Pending
        }
    }
    impl Drop for PendingDrop {
        fn drop(&mut self) {
            self.0
                .store(NativeQueryScope::current().is_some(), Ordering::Release);
        }
    }
    let cancelled = query
        .begin(
            QueryObservationKind::Transaction,
            QueryObservationPhase::Steady,
            None,
        )?
        .expect("cancellation guard");
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    drop(cancelled.scope(PendingDrop(entered.clone())));
    assert!(
        entered.load(Ordering::Acquire),
        "future Drop enters the host query scope"
    );
    drop(cancelled);
    assert!(binding.state.queries.lock().unwrap().entries.is_empty());
    Ok(())
}

#[test]
fn query_scope_allows_nested_map_callbacks_and_rejects_busy_handle() -> anyhow::Result<()> {
    let (_graph, scope) = setup(true)?;
    let (v2, v4) = extensions();
    let binding = scope
        .in_scope(|| NativeTelemetryBinding::capture_query(Some(v2), Some(v4), "query"))?
        .expect("binding");
    let token = begin(&binding)?;
    struct Callback<'a> {
        binding: &'a NativeTelemetryBinding,
        token: QueryToken,
    }
    unsafe extern "C" fn invoke(state: *mut c_void) -> abi::Status {
        transport::status_boundary(|| {
            let state = unsafe { &*state.cast::<Callback<'_>>() };
            assert!(NativeQueryScope::current().is_some());
            // This must not deadlock on the token-map lock held by the scope.
            let nested = begin(state.binding).map_err(Failure::from)?;
            assert_ne!(nested.lease, state.token.lease);
            assert!(
                unsafe {
                    transport::take_status(query_scope(
                        state.binding.table().context,
                        state.token,
                        invoke,
                        std::ptr::null_mut(),
                    ))
                }
                .is_err(),
                "busy tokens reject before invoking the callback"
            );
            let result = encode_telemetry(
                &QueryObservationResult {
                    outcome: QueryObservationOutcome::Success,
                    result_count: None,
                },
                MAX_TELEMETRY_RECORD_BYTES,
            )
            .map_err(|error| Failure::from(anyhow::Error::from(error)))?;
            assert!(
                unsafe {
                    transport::take_status(finish_query(
                        state.binding.table().context,
                        state.token,
                        abi::BorrowedBytes::new(&result),
                    ))
                }
                .is_err(),
                "busy finish consumes the token and defers cancellation"
            );
            Ok(())
        })
    }
    let mut callback = Callback {
        binding: &binding,
        token,
    };
    unsafe {
        transport::take_status(query_scope(
            binding.table().context,
            token,
            invoke,
            (&mut callback as *mut Callback<'_>).cast(),
        ))?
    };
    assert_eq!(binding.state.queries.lock().unwrap().entries.len(), 1);
    assert_eq!(binding.state.query_count.load(Ordering::Acquire), 1);
    let retained = binding.state.clone();
    drop(binding);
    assert!(retained.queries.lock().unwrap().entries.is_empty());
    Ok(())
}

#[test]
fn nested_sdk_scopes_reject_reentry_without_blocking() -> anyhow::Result<()> {
    let (done, completed) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let result = (|| -> anyhow::Result<()> {
            let (_graph, scope) = setup(true)?;
            let (v2, v4) = extensions();
            let binding = scope
                .in_scope(|| NativeTelemetryBinding::capture_query(Some(v2), Some(v4), "query"))?
                .expect("binding");
            let query = binding.scope(|| Ok(QueryTelemetry::capture()?))?;
            let mut first = query
                .begin(
                    QueryObservationKind::Transaction,
                    QueryObservationPhase::Steady,
                    None,
                )?
                .expect("first scope");
            let mut second = query
                .begin(
                    QueryObservationKind::Transaction,
                    QueryObservationPhase::Steady,
                    None,
                )?
                .expect("second scope");
            first.in_scope(&mut || {
                second
                    .in_scope(&mut || {
                        let error = first
                            .in_scope(&mut || panic!("busy scope executed"))
                            .unwrap_err();
                        assert!(error.to_string().contains("busy"), "{error}");
                    })
                    .unwrap();
            })?;
            first.finish(QueryObservationOutcome::Success, None)?;
            second.finish(QueryObservationOutcome::Success, None)?;
            assert!(binding.state.queries.lock().unwrap().entries.is_empty());
            Ok(())
        })();
        let _ = done.send(result.map_err(|error| error.to_string()));
    });
    completed
        .recv_timeout(std::time::Duration::from_secs(5))?
        .map_err(anyhow::Error::msg)?;
    worker.join().expect("query scope worker did not panic");
    Ok(())
}

#[test]
fn query_scope_releases_retirement_gates_before_foreign_callbacks() -> anyhow::Result<()> {
    let (done, completed) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let result = (|| -> anyhow::Result<()> {
            let (_graph, scope) = setup(true)?;
            let (v2, v4) = extensions();
            let mut binding = Some(
                scope
                    .in_scope(|| {
                        NativeTelemetryBinding::capture_query(Some(v2), Some(v4), "query")
                    })?
                    .expect("binding"),
            );
            let query = binding
                .as_ref()
                .unwrap()
                .scope(|| Ok(QueryTelemetry::capture()?))?;
            let mut guard = query
                .begin(
                    QueryObservationKind::Transaction,
                    QueryObservationPhase::Steady,
                    None,
                )?
                .expect("scope");
            let retained = binding.as_ref().unwrap().state.clone();
            guard.in_scope(&mut || {
                drop(binding.take());
                assert!(
                    query
                        .event(QueryObservationEvent::Input {
                            phase: QueryObservationPhase::Steady,
                        })
                        .is_err(),
                    "retired binding accepted a late query update"
                );
            })?;
            assert!(guard
                .finish(QueryObservationOutcome::Success, None)
                .is_err());
            drop(guard);
            assert!(retained.queries.lock().unwrap().entries.is_empty());
            assert_eq!(retained.query_count.load(Ordering::Acquire), 0);
            Ok(())
        })();
        let _ = done.send(result.map_err(|error| error.to_string()));
    });
    completed
        .recv_timeout(std::time::Duration::from_secs(5))?
        .map_err(anyhow::Error::msg)?;
    worker
        .join()
        .expect("query retirement worker did not panic");
    Ok(())
}

#[test]
fn missing_query_extension_is_optional_unless_explicitly_required() -> anyhow::Result<()> {
    let (_graph, scope) = setup(false)?;
    let (v2, _) = extensions();
    let binding = scope
        .in_scope(|| NativeTelemetryBinding::capture_query(Some(v2), None, "query"))?
        .expect("legacy binding");
    let query = binding.scope(|| Ok(QueryTelemetry::capture()?))?;
    assert!(!query.enabled());
    assert!(query
        .begin(
            QueryObservationKind::Evaluate,
            QueryObservationPhase::Steady,
            Some(1)
        )?
        .is_none());
    query.event(QueryObservationEvent::Input {
        phase: QueryObservationPhase::Steady,
    })?;
    let (_required_graph, required_scope) = setup(true)?;
    assert!(required_scope
        .in_scope(|| NativeTelemetryBinding::capture_query(Some(v2), None, "query"))
        .is_err());
    Ok(())
}

struct QueryFactory(bool);

impl sdk::Factory for QueryFactory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        let schema = GraphChangeCodec::schema();
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new("query-probe", "1").unwrap(),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: Default::default(),
            ports: vec![
                PortDescriptor::new(
                    PortId::try_new("in").unwrap(),
                    PortDirection::Input,
                    schema.descriptor().clone(),
                    Default::default(),
                ),
                PortDescriptor::new(
                    PortId::try_new("out").unwrap(),
                    PortDirection::Output,
                    schema.descriptor().clone(),
                    Default::default(),
                ),
            ],
            completion: None,
            capabilities: sdk::Capabilities {
                semantic_kind: Some(ComponentSemanticKind::Query),
                ..Default::default()
            },
        }
    }

    fn supports_query_instrumentation(&self) -> bool {
        self.0
    }

    fn create(
        &self,
        _: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        panic!("required-capability validation must precede construction")
    }
}

#[test]
fn v4_export_does_not_claim_query_factory_instrumentation() -> anyhow::Result<()> {
    for instrumented in [false, true] {
        let plugin =
            crate::computation::NativePlugin::from_definition(sdk::PluginDefinition::new(
                "query-probe",
                "1",
                vec![Arc::new(QueryFactory(instrumented))],
                vec![GraphChangeCodec::schema()],
            )?)?;
        let factory = &plugin.factories()[0];
        assert_eq!(
            factory.telemetry_capabilities() & QUERY_OBSERVATIONS != 0,
            instrumented
        );
        if !instrumented {
            let (_graph, scope) = setup(true)?;
            let error = scope
                .in_scope(|| {
                    factory.create_component(
                        ComponentId::try_new("d5bd71eb-52ae-4abb-a435-f707c5376509")?,
                        serde_json::json!({}),
                    )
                })
                .err()
                .expect("required query telemetry rejects the uninstrumented factory");
            assert!(
                error
                    .to_string()
                    .contains("required native query observations"),
                "{error:#}"
            );
        }
    }
    Ok(())
}
