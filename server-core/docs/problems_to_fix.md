
- **Finding**: Application sources cannot restart after `stop()` because `process_events()` consumes the channel and subsequent `start()` calls hit "Receiver already taken" (`server-core/src/sources/application/mod.rs`). Impact: any stop/restart cycle leaves application sources permanently offline. Recommendation: repopulate `app_rx` during stop, or recreate the channel on start so restart paths succeed.

- **Finding**: Event-processing tasks spawned in `LifecycleManager::start_event_processors()` have no handle or shutdown path (`server-core/src/lifecycle.rs`). Impact: tasks live for the lifetime of the process with no ability to quiesce or surface metrics, complicating orderly shutdown and testing. Recommendation: retain task handles, expose a shutdown signal, or move the logging into a managed service component.

- **Finding**: Bootstrap replay tasks spawned per source are detached (`tokio::spawn` without handle) and cannot be cancelled on stop (`server-core/src/queries/manager.rs`). Impact: long-running or stuck bootstrap work continues after stop/remove, leaking resources. Recommendation: capture the join handles alongside `subscription_tasks`, cancel them during stop, and surface completion status.

- **Finding**: Result serialization converts large integers/floats to strings (`convert_query_variables_to_json` in `server-core/src/queries/manager.rs`). Impact: downstream consumers lose numeric type fidelity and must re-parse strings. Recommendation: map values to `serde_json::Number` where possible and fall back to strings only when JSON cannot represent the magnitude.

- **Finding**: `SourceManager::update_source()` stops the instance, deletes it, and then re-adds the new config with no rollback (`server-core/src/sources/manager.rs`). Impact: a failure during re-add leaves the source missing and its handles orphaned. Recommendation: stage the new source first, swap under the write-lock, and roll back on failure.

- **Finding**: Application source bootstrap cache (`bootstrap_data`) grows unbounded and is never pruned (`server-core/src/sources/application/mod.rs`). Impact: long-lived sources accumulate every insert ever seen, eventually exhausting memory. Recommendation: make retention configurable (max items, TTL) or stream bootstrap from durable storage instead of keeping all events in RAM.

- **Finding**: Shutdown paths rely on fixed sleeps (100 ms) after stop requests (`server-core/src/sources/manager.rs`, `server-core/src/queries/manager.rs`, `server-core/src/reactions/manager.rs`). Impact: racy validation—slow components may still be running after the sleep, while fast ones waste time. Recommendation: poll status with backoff or await explicit completion signals from the component tasks.

- **Finding**: Bootstrap provider factory requires `query_api_url` up front for platform sources, but the provider later attempts to infer it from source properties (`server-core/src/bootstrap/mod.rs`). Impact: configuration that relies on source-level defaults trips the factory error path. Recommendation: defer validation until the property lookup runs or plumb the property into the factory constructor.


- **Finding**: `map_component_error` and `map_state_error` rely on substring matching of error messages (`server-core/src/component_ops.rs`). Impact: fragile error classification—changes to upstream wording break mapping, leading to misleading `DrasiError`s. Recommendation: define dedicated error enums in the managers and perform structured matching.

- **Finding**: Mixed use of `anyhow::Error` and `DrasiError` means stack context is frequently stringified and lost (`server-core/src/server_core.rs`, managers). Impact: diagnostics are hard to correlate with source code, and tracing lacks structured data. Recommendation: convert manager methods to return typed errors and log context where created instead of late stringification.


- **Finding**: Lifecycle logging emits multiple `info!` statements per component start/stop (`server-core/src/lifecycle.rs`). Impact: production logs become dominated by lifecycle chatter, obscuring actionable events. Recommendation: consolidate messages or move repetitive logs to `debug!`, keeping a single structured `info!` per phase.


- **Finding**: There are no regression tests covering stop/start cycles for application sources or queries (`server-core/src/sources/tests.rs`, `server-core/src/queries/tests.rs`). Impact: the restart bugs above slip through CI. Recommendation: add async integration tests that stop and restart each component type, asserting that handles remain functional.

- **Finding**: No automated checks exist for bootstrap memory pressure or queue overflow metrics (`server-core/src/queries/tests.rs`, `server-core/src/channels/events_test.rs`). Impact: performance regressions go unnoticed until runtime. Recommendation: extend tests or benchmarks to assert bounded growth and validate metrics reporting.
