// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_lib::{
    computation::v1::{ComputationGraph, GraphSelection},
    observability::{
        ComponentLogKey, ComponentLogRuntime, ComponentLogType, LogPolicy, LoggingError,
        TelemetryOptions, TelemetryProfile, TelemetrySettings, TELEMETRY_TARGET,
    },
    DrasiLib,
};
use std::{
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tracing_subscriber::{filter::filter_fn, prelude::*};

fn isolated(name: &str, run: impl FnOnce()) -> Option<String> {
    if std::env::var("DRASI_FOUNDATIONS_TEST").as_deref() == Ok(name) {
        run();
        return None;
    }
    let output = Command::new(std::env::current_exe().expect("current test binary"))
        .args(["--exact", name, "--nocapture"])
        .env("DRASI_FOUNDATIONS_TEST", name)
        .env_remove("RUST_LOG")
        .output()
        .expect("run isolated logging test");
    assert!(
        output.status.success(),
        "child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8(output.stdout).expect("test output is UTF-8"))
}

fn run_async(work: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread test runtime")
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(15), work)
                .await
                .expect("logging test completed before its deadline");
        });
}

fn log_component(instance: &str) {
    let span = tracing::info_span!(
        "computation_component",
        instance_id = instance,
        component_id = "shared",
        component_type = "source"
    );
    let _entered = span.enter();
    tracing::info!("DRASI_DEFAULT_TRACING_LOG");
    log::info!("DRASI_DEFAULT_LOG_BRIDGE");
}

#[test]
fn automatic_default_logging_and_shared_shutdown() {
    let output = isolated("automatic_default_logging_and_shared_shutdown", || {
        run_async(async {
            let (one, two) = tokio::join!(
                DrasiLib::builder().with_id("one").build(),
                DrasiLib::builder().with_id("two").build(),
            );
            let one = one.unwrap();
            let two = two.unwrap();
            assert!(!one.telemetry().policy().enabled());
            let logging = one.component_logging().unwrap().clone();
            assert!(Arc::ptr_eq(
                &logging.registry(),
                &two.component_logging().unwrap().registry()
            ));
            log_component("one");
            log_component("two");
            logging.flush().await.unwrap();
            for instance in ["one", "two"] {
                let key = ComponentLogKey::new(instance, ComponentLogType::Source, "shared");
                let history = logging.registry().get_history_by_key(&key).await;
                assert_eq!(history.len(), 2);
                assert_eq!(history[0].instance_id, instance);
            }
            assert!(metrics::set_global_recorder(metrics::NoopRecorder).is_ok());
            one.shutdown().await.unwrap();
            assert!(!logging.is_closed());
            log_component("two");
            logging.flush().await.unwrap();
            let key = ComponentLogKey::new("two", ComponentLogType::Source, "shared");
            assert_eq!(logging.registry().log_count_by_key(&key).await, 4);
            two.shutdown().await.unwrap();
            logging.shutdown(Duration::from_secs(2)).await.unwrap();
            assert!(matches!(
                drasi_lib::init_default_subscriber(),
                Err(LoggingError::Closed)
            ));
            assert!(DrasiLib::builder().build().await.is_err());
        })
    });
    if let Some(output) = output {
        assert!(output.contains("DRASI_DEFAULT_TRACING_LOG"));
        assert!(output.contains("DRASI_DEFAULT_LOG_BRIDGE"));
    }
}

#[test]
fn existing_subscriber_is_not_replaced() {
    isolated("existing_subscriber_is_not_replaced", || {
        run_async(async {
            tracing::subscriber::set_global_default(tracing_subscriber::registry()).unwrap();
            let error = match DrasiLib::builder().build().await {
                Ok(_) => {
                    panic!("automatic setup must not claim capture under an unrelated subscriber")
                }
                Err(error) => error,
            };
            assert!(matches!(
                error.downcast_ref::<LoggingError>(),
                Some(LoggingError::SubscriberConflict)
            ));
            assert!(tracing::dispatcher::get_default(|dispatch| {
                dispatch.is::<tracing_subscriber::Registry>()
            }));
        })
    });
}

struct HostLogger(AtomicUsize);
impl log::Log for HostLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, _: &log::Record<'_>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn flush(&self) {}
}
static HOST_LOGGER: HostLogger = HostLogger(AtomicUsize::new(0));

#[test]
fn existing_logger_is_not_replaced() {
    isolated("existing_logger_is_not_replaced", || {
        run_async(async {
            log::set_logger(&HOST_LOGGER).unwrap();
            log::set_max_level(log::LevelFilter::Info);
            let error = match DrasiLib::builder().build().await {
                Ok(_) => panic!("automatic setup must not replace the host logger"),
                Err(error) => error,
            };
            assert!(matches!(
                error.downcast_ref::<LoggingError>(),
                Some(LoggingError::LoggerConflict)
            ));
            log::info!("host logger remains installed");
            assert_eq!(HOST_LOGGER.0.load(Ordering::SeqCst), 1);
            tracing::subscriber::set_global_default(tracing_subscriber::registry()).unwrap();
        })
    });
}

struct TraceCounter(Arc<AtomicUsize>);
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for TraceCounter {
    fn on_new_span(
        &self,
        _: &tracing::span::Attributes<'_>,
        _: &tracing::span::Id,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn custom_capture_can_disable_logs_without_disabling_traces() {
    isolated(
        "custom_capture_can_disable_logs_without_disabling_traces",
        || {
            run_async(async {
                let logging = ComponentLogRuntime::new(LogPolicy::new("off").unwrap()).unwrap();
                let settings = TelemetrySettings::new(TelemetryProfile::Debug).with_metrics(false);
                let policy = settings.resolve().unwrap();
                let traces = Arc::new(AtomicUsize::new(0));
                tracing_subscriber::registry()
                    .with(logging.layer())
                    .with(
                        TraceCounter(traces.clone()).with_filter(filter_fn(move |metadata| {
                            metadata.is_span()
                                && metadata.target() == TELEMETRY_TARGET
                                && metadata.name() == "query.evaluate"
                                && policy.allows_span(metadata.name())
                        })),
                    )
                    .try_init()
                    .unwrap();
                let core = DrasiLib::builder()
                    .with_id("custom")
                    .with_component_logging(logging.clone())
                    .with_telemetry(settings)
                    .build()
                    .await
                    .unwrap();
                assert!(core.telemetry().policy().tracing_enabled());
                assert!(!core.telemetry().policy().metrics_enabled());
                assert_eq!(core.telemetry().scope().unwrap().instance_id(), "custom");
                log_component("custom");
                let span = tracing::debug_span!(target: TELEMETRY_TARGET, "query.evaluate");
                drop(span);
                logging.flush().await.unwrap();
                let key = ComponentLogKey::new("custom", ComponentLogType::Source, "shared");
                assert_eq!(logging.registry().log_count_by_key(&key).await, 0);
                assert_eq!(traces.load(Ordering::SeqCst), 1);
                assert!(drasi_lib::observability::default_log_level()
                    .unwrap()
                    .is_none());
                core.shutdown().await.unwrap();
                assert!(!logging.is_closed());
                logging.shutdown(Duration::from_secs(2)).await.unwrap();
            })
        },
    );
}

#[test]
fn direct_graph_identity_is_runtime_only_and_construction_is_passive() {
    isolated(
        "direct_graph_identity_is_runtime_only_and_construction_is_passive",
        || {
            let off = ComputationGraph::builder("graph").build().unwrap();
            let settings = TelemetrySettings::new(TelemetryProfile::Basic).with_metrics(false);
            assert!(ComputationGraph::builder("graph")
                .with_telemetry(settings.clone())
                .build()
                .is_err());
            let on = ComputationGraph::builder("graph")
                .with_telemetry(TelemetryOptions::new(settings).with_instance_id("host"))
                .build()
                .unwrap();
            assert_eq!(on.telemetry().scope().unwrap().instance_id(), "host");
            assert_eq!(on.telemetry().scope().unwrap().graph_id(), "graph");
            assert_eq!(
                serde_json::to_value(off.snapshot().select(GraphSelection::All).unwrap()).unwrap(),
                serde_json::to_value(on.snapshot().select(GraphSelection::All).unwrap()).unwrap(),
            );
            tracing::subscriber::set_global_default(tracing_subscriber::registry()).unwrap();
            log::set_logger(&HOST_LOGGER).unwrap();
            assert!(metrics::set_global_recorder(metrics::NoopRecorder).is_ok());
        },
    );
}

#[test]
fn invalid_settings_fail_before_global_installation() {
    isolated("invalid_settings_fail_before_global_installation", || {
        run_async(async {
            assert!(DrasiLib::builder()
                .with_telemetry(
                    TelemetryOptions::default().with_trace_directives("target=not-a-level")
                )
                .build()
                .await
                .is_err());
            assert!(DrasiLib::builder()
                .with_telemetry(TelemetryProfile::Basic)
                .build()
                .await
                .is_err());
            tracing::subscriber::set_global_default(tracing_subscriber::registry()).unwrap();
            log::set_logger(&HOST_LOGGER).unwrap();
            assert!(metrics::set_global_recorder(metrics::NoopRecorder).is_ok());
        })
    });
}
