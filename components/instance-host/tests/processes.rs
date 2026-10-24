// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt},
        net::UnixStream,
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

use anyhow::{ensure, Context, Result};
use drasi_instance_host::{
    config::{ConfigurationStorage, NativePlugin},
    protocol::{OperationPhase, OperationStatus},
    ChildPhase, ControlClient, ControlCommand, ControlReply, Credentials, HostConfig,
    HostObservation, HostPhase, StopStage, SupervisorFactory,
};
use drasi_lib::computation::v1::*;
use serde_json::json;
use uuid::Uuid;

const WATCHDOG: Duration = Duration::from_secs(25);

#[path = "network/mod.rs"]
mod network;

struct Directory(PathBuf);
impl Directory {
    fn new() -> Result<Self> {
        let root = std::env::var_os("DRASI_INSTANCE_HOST_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .to_path_buf()
            });
        let path = root.join(format!(".ih-{}", &Uuid::new_v4().simple().to_string()[..8]));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
    fn join(&self, path: &str) -> PathBuf {
        self.0.join(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct HostProcess {
    child: Child,
    state: PathBuf,
    diagnostics: PathBuf,
}

impl HostProcess {
    fn spawn(config: &HostConfig, directory: &Directory) -> Result<Self> {
        config.validate()?;
        let manifest = directory.join(&format!("{}.json", config.instance_id));
        let diagnostics = directory.join(&format!("{}.log", config.instance_id));
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&manifest)?;
        file.write_all(&serde_json::to_vec(config)?)?;
        let child = Command::new(env!("CARGO_BIN_EXE_drasi-instance-host"))
            .args(["--config"])
            .arg(manifest)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&diagnostics)?)
            .env("DRASI_INSTANCE_FIXTURE_LOAD_LOG", directory.join("loads"))
            .process_group(0)
            .spawn()?;
        Ok(Self {
            child,
            state: config.state_dir.clone(),
            diagnostics,
        })
    }

    fn client(&self) -> Result<ControlClient> {
        let mut client = ControlClient::new(Credentials::load(&self.state)?);
        client.timeout = Duration::from_secs(2);
        Ok(client)
    }

    fn status(&self) -> Result<HostObservation> {
        self.client()?.status()
    }

    fn wait(&mut self, predicate: impl Fn(&HostObservation) -> bool) -> Result<HostObservation> {
        let deadline = Instant::now() + WATCHDOG;
        let mut last = None;
        while Instant::now() < deadline {
            if let Some(exit) = self.child.try_wait()? {
                anyhow::bail!(
                    "host exited {exit}: {}",
                    std::fs::read_to_string(&self.diagnostics).unwrap_or_default()
                );
            }
            if let Ok(observation) = self.status() {
                if predicate(&observation) {
                    return Ok(observation);
                }
                last = Some(observation);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        anyhow::bail!(
            "host watchdog expired; last observation={last:?}; stderr={}",
            std::fs::read_to_string(&self.diagnostics).unwrap_or_default()
        )
    }

    fn operation(&self, command: ControlCommand) -> Result<Uuid> {
        let id = Uuid::new_v4();
        ensure!(
            matches!(self.client()?.request(id, command)?, ControlReply::Accepted { operation_id } if operation_id == id),
            "operation not accepted"
        );
        Ok(id)
    }

    fn operation_status(&self, operation_id: Uuid) -> Result<OperationStatus> {
        match self
            .client()?
            .request(Uuid::new_v4(), ControlCommand::Operation { operation_id })?
        {
            ControlReply::Operation { operation } => Ok(operation),
            _ => anyhow::bail!("unexpected operation response"),
        }
    }

    fn wait_operation(&self, id: Uuid) -> Result<OperationStatus> {
        let deadline = Instant::now() + WATCHDOG;
        while Instant::now() < deadline {
            let status = self.operation_status(id)?;
            if matches!(
                status.phase,
                OperationPhase::Completed | OperationPhase::Failed | OperationPhase::Incomplete
            ) {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        anyhow::bail!("operation watchdog expired")
    }

    fn wait_exit(&mut self) -> Result<ExitStatus> {
        let deadline = Instant::now() + WATCHDOG;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        anyhow::bail!(
            "host process did not exit; {}",
            std::fs::read_to_string(&self.diagnostics).unwrap_or_default()
        )
    }

    fn kill(&mut self) -> Result<()> {
        self.child.kill()?;
        self.wait_exit()?;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.operation(ControlCommand::StopTree)?;
        ensure!(self.wait_exit()?.success(), "normal host shutdown failed");
        Ok(())
    }
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        // Scoped to this test's process group; descendants have their own
        // inherited ownership pipe and independent watchdog.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn id(name: &str) -> ComponentId {
    ComponentId::try_new(name).unwrap()
}

fn add(config: &mut HostConfig, specification: ComponentSpecification) {
    config.desired.topology.components.push(DesiredComponent {
        descriptor: specification.descriptor.clone(),
        role: specification.role,
        completion: specification.completion,
        streams: BTreeMap::new(),
        lifecycle: LifecyclePolicy::default(),
        input_merge: InputMergePolicy::default(),
        construction: ComponentConstruction::Factory(specification),
    });
}

fn child(parent: &mut HostConfig, name: &str, config: &HostConfig) -> Result<()> {
    add(parent, SupervisorFactory::specification(id(name), config)?);
    Ok(())
}

fn fixture() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("DRASI_INSTANCE_HOST_FIXTURE") {
        return Ok(path.into());
    }
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_drasi-instance-host"));
    let name = if cfg!(target_os = "macos") {
        "libinstance_host_fixture.dylib"
    } else {
        "libinstance_host_fixture.so"
    };
    let path = executable.parent().unwrap().join("examples").join(name);
    ensure!(path.is_file(), "build the real native fixture first: cargo build -p drasi-instance-host --features dynamic-plugin --example instance_host_fixture ({})", path.display());
    Ok(path)
}

fn native(
    config: &mut HostConfig,
    mode: &str,
    marker: &Path,
    start_ms: u64,
    stop_ms: u64,
) -> Result<()> {
    config.plugins.push(NativePlugin {
        path: fixture()?,
        sha256: None,
    });
    let mut implementation = ImplementationIdentity::try_new("fixture/instance-host", "1")?;
    implementation.plugin = Some(PluginIdentity {
        id: "instance-host-fixture".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    });
    add(
        config,
        ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(id("native"), Vec::new())?
                .with_plugin_identity(implementation.plugin.clone().expect("fixture provenance"))?,
            role: ComponentRole::Service,
            completion: None,
            implementation,
            configuration_version: 1,
            configuration: BTreeMap::from([
                ("mode".into(), ConfigurationValue::Literal(json!(mode))),
                ("marker".into(), ConfigurationValue::Literal(json!(marker))),
                (
                    "start_delay_ms".into(),
                    ConfigurationValue::Literal(json!(start_ms)),
                ),
                (
                    "stop_delay_ms".into(),
                    ConfigurationValue::Literal(json!(stop_ms)),
                ),
            ]),
            dependencies: BTreeMap::new(),
        },
    );
    Ok(())
}

fn gone(pid: u32) -> Result<()> {
    let deadline = Instant::now() + WATCHDOG;
    while Instant::now() < deadline {
        if unsafe { libc::kill(pid as i32, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    anyhow::bail!("owned descendant {pid} survived its owner")
}

#[test]
fn same_normal_host_hierarchy_uses_child_only_native_library_and_stable_namespaces() -> Result<()> {
    let directory = Directory::new()?;
    let mut grandchild = HostConfig::empty("grandchild", directory.join("c"));
    native(&mut grandchild, "healthy", &directory.join("native"), 0, 0)?;
    let mut child_config = HostConfig::empty("child", directory.join("b"));
    child(&mut child_config, "grandchild-owner", &grandchild)?;
    let mut config = HostConfig::empty("parent", directory.join("a"));
    child(&mut config, "child-owner", &child_config)?;
    let mut parent = HostProcess::spawn(&config, &directory)?;
    let status = parent.wait(|status| status.ready)?;
    assert!(status.loaded_plugins.is_empty());
    assert_eq!(status.state_dir, config.state_dir);
    let b = &status.children[0];
    let child_status = b.last_known.as_ref().context("child observation")?;
    assert_eq!(child_status.instance_id, "child");
    assert!(child_status.loaded_plugins.is_empty());
    let c = &child_status.children[0];
    let grandchild_status = c.last_known.as_ref().context("grandchild observation")?;
    assert_eq!(grandchild_status.loaded_plugins, ["instance-host-fixture"]);
    assert_eq!(
        std::fs::read_to_string(directory.join("loads"))?.trim(),
        grandchild_status.pid.to_string()
    );
    assert_eq!(grandchild_status.state_dir, grandchild.state_dir);
    assert_ne!(status.generation, child_status.generation);
    assert_ne!(child_status.generation, grandchild_status.generation);
    let original_generation = status.generation;
    let original_fingerprint = status.manifest_fingerprint.clone();
    let child_pid = child_status.pid;
    let grandchild_pid = grandchild_status.pid;
    parent.stop()?;
    gone(child_pid)?;
    gone(grandchild_pid)?;
    let mut restarted = HostProcess::spawn(&config, &directory)?;
    let status = restarted.wait(|status| status.ready)?;
    assert_ne!(status.generation, original_generation);
    assert_eq!(status.manifest_fingerprint, original_fingerprint);
    assert_eq!(
        status.children[0].phase,
        ChildPhase::Stopped,
        "persistent stopped intent must not auto-restart a child"
    );
    assert_eq!(
        std::fs::read_to_string(directory.join("loads"))?
            .lines()
            .count(),
        1
    );
    restarted.stop()
}

#[test]
fn initialization_failure_is_not_readiness_and_keeps_native_desired_definition() -> Result<()> {
    let directory = Directory::new()?;
    let mut child_config = HostConfig::empty("child", directory.join("b"));
    native(
        &mut child_config,
        "start-error",
        &directory.join("failed"),
        0,
        0,
    )?;
    let mut config = HostConfig::empty("parent", directory.join("a"));
    child(&mut config, "child-owner", &child_config)?;
    let mut parent = HostProcess::spawn(&config, &directory)?;
    let status = parent.wait(|status| {
        status
            .children
            .first()
            .is_some_and(|child| child.phase == ChildPhase::Failed)
    })?;
    assert!(!status.ready);
    let observation = status.children[0]
        .last_known
        .as_ref()
        .context("failed child remains observable")?;
    assert!(!observation.ready);
    assert!(observation.components.iter().any(|component| component
        .failure
        .as_deref()
        .is_some_and(|failure| failure.contains("deliberate native initialization failure"))));
    let credentials = Credentials::load(&child_config.state_dir)?;
    let client = ControlClient::new(credentials);
    let ControlReply::Configuration { configuration } =
        client.request(Uuid::new_v4(), ControlCommand::Configuration)?
    else {
        panic!("configuration reply")
    };
    assert_eq!(configuration.desired.topology.components.len(), 1);
    let old_generation = status.children[0].generation;
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        parent.status()?.children[0].generation,
        old_generation,
        "no implicit crash restart"
    );
    parent.stop()
}

#[test]
fn accepted_stop_survives_disconnected_caller_and_force_reaps_pending_native_cleanup() -> Result<()>
{
    let directory = Directory::new()?;
    let mut child_config = HostConfig::empty("child", directory.join("b"));
    child_config.runtime.operation_timeout_ms = 300;
    native(
        &mut child_config,
        "pending-stop",
        &directory.join("pending"),
        0,
        0,
    )?;
    let mut config = HostConfig::empty("parent", directory.join("a"));
    config.runtime.operation_timeout_ms = 500;
    child(&mut config, "child-owner", &child_config)?;
    let mut parent = HostProcess::spawn(&config, &directory)?;
    let status = parent.wait(|status| status.ready)?;
    let generation = status.children[0].generation.unwrap();
    let pid = status.children[0].pid.unwrap();
    let credentials = parent.client()?.credentials;
    let operation_id = Uuid::new_v4();
    let request = serde_json::to_vec(&json!({
        "version":1, "instance_id":credentials.instance_id, "generation":credentials.generation,
        "token":credentials.token, "request_id":operation_id,
        "command":{"kind":"stop_child", "component":"child-owner", "generation":generation}
    }))?;
    let mut socket = UnixStream::connect(&credentials.endpoint)?;
    socket.write_all(&(request.len() as u32).to_be_bytes())?;
    socket.write_all(&request)?;
    drop(socket);
    parent.wait(|status| status.children[0].phase == ChildPhase::CleanupRequired)?;
    assert!(
        unsafe { libc::kill(pid as i32, 0) } == 0,
        "timed-out stop must retain, not silently kill"
    );
    let forced = parent.operation(ControlCommand::ForceChild {
        component: "child-owner".into(),
        generation,
    })?;
    let outcome = parent.wait_operation(forced)?;
    assert_eq!(outcome.phase, OperationPhase::Completed, "{outcome:?}");
    let status = parent.wait(|status| status.children[0].reaped)?;
    assert_eq!(status.children[0].phase, ChildPhase::Terminated);
    gone(pid)?;
    parent.stop()
}

#[test]
fn terminal_crash_requires_explicit_repair_and_stale_generation_cannot_kill_replacement(
) -> Result<()> {
    let directory = Directory::new()?;
    let child_config = HostConfig::empty("child", directory.join("b"));
    let mut config = HostConfig::empty("parent", directory.join("a"));
    child(&mut config, "child-owner", &child_config)?;
    let mut parent = HostProcess::spawn(&config, &directory)?;
    let status = parent.wait(|status| status.ready)?;
    let old = status.children[0].clone();
    let old_credentials = Credentials::load(&child_config.state_dir)?;
    unsafe {
        libc::kill(old.pid.unwrap() as i32, libc::SIGKILL);
    }
    parent.wait(|status| {
        status.children[0].phase == ChildPhase::Failed && status.children[0].reaped
    })?;
    let repair = parent.operation(ControlCommand::RepairChild {
        component: "child-owner".into(),
        generation: old.generation.unwrap(),
    })?;
    let operation = parent.wait_operation(repair)?;
    assert_eq!(operation.phase, OperationPhase::Completed, "{operation:?}");
    let replaced = parent.wait(|status| status.children[0].phase == ChildPhase::Ready)?;
    assert_ne!(replaced.children[0].generation, old.generation);
    assert_eq!(
        replaced.children[0].manifest_fingerprint,
        old.manifest_fingerprint
    );
    assert!(ControlClient::new(old_credentials)
        .request(Uuid::new_v4(), ControlCommand::StopTree)
        .is_err());
    let stale = parent.operation(ControlCommand::ForceChild {
        component: "child-owner".into(),
        generation: old.generation.unwrap(),
    })?;
    assert_ne!(
        parent.wait_operation(stale)?.phase,
        OperationPhase::Completed
    );
    assert_eq!(
        parent.status()?.children[0].generation,
        replaced.children[0].generation
    );
    parent.stop()
}

#[test]
fn abrupt_parent_death_terminates_noncooperative_grandchild_without_graph_heartbeat() -> Result<()>
{
    let directory = Directory::new()?;
    let mut grandchild = HostConfig::empty("grandchild", directory.join("c"));
    grandchild.runtime.worker_threads = 1;
    native(&mut grandchild, "block", &directory.join("busy"), 0, 0)?;
    let mut child_config = HostConfig::empty("child", directory.join("b"));
    child(&mut child_config, "grandchild-owner", &grandchild)?;
    let mut config = HostConfig::empty("parent", directory.join("a"));
    child(&mut config, "child-owner", &child_config)?;
    let mut parent = HostProcess::spawn(&config, &directory)?;
    let deadline = Instant::now() + WATCHDOG;
    while !directory.join("busy.blocked").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    ensure!(
        directory.join("busy.blocked").exists(),
        "fixture did not enter genuine noncooperative native run"
    );
    let grandchild_pid: u32 = std::fs::read_to_string(directory.join("busy.blocked"))?.parse()?;
    let child_pid = ControlClient::new(Credentials::load(&child_config.state_dir)?)
        .status()?
        .pid;
    // Health/control is independently serviceable even with one blocked graph worker.
    let busy = ControlClient::new(Credentials::load(&grandchild.state_dir)?).status()?;
    assert_eq!(busy.pid, grandchild_pid);
    parent.kill()?;
    gone(child_pid)?;
    gone(grandchild_pid)?;
    Ok(())
}

#[test]
fn plugin_allowlist_failure_remains_an_observable_host_not_a_ready_listening_socket() -> Result<()>
{
    let directory = Directory::new()?;
    let mut config = HostConfig::empty("bad-plugin", directory.join("a"));
    config.plugins.push(NativePlugin {
        path: directory.join("not-a-plugin.dylib"),
        sha256: None,
    });
    let mut host = HostProcess::spawn(&config, &directory)?;
    let status = host.wait(|status| status.phase == HostPhase::Failed)?;
    assert!(!status.ready);
    assert!(!status.healthy);
    assert!(status
        .error
        .as_deref()
        .is_some_and(|error| error.contains("initialize normal DrasiLib")));
    assert_eq!(status.manifest_fingerprint, config.fingerprint()?);
    host.stop()
}

#[test]
fn persisted_normal_management_keeps_waiting_and_failed_state_and_excludes_duplicate_owner(
) -> Result<()> {
    for mode in ["missing-factory", "unknown-mode", "start-error"] {
        let directory = Directory::new()?;
        let key_path = directory.join("key");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key_path)?
            .write_all(&[73; 32])?;
        let mut config = HostConfig::empty("persistent", directory.join("a"));
        config.configuration_store = Some(ConfigurationStorage { key_file: key_path });
        let mut changed = config.clone();
        let component = if mode == "missing-factory" {
            add(
                &mut changed,
                ComponentSpecification {
                    descriptor: ComponentDescriptor::try_new(id("unavailable"), Vec::new())?,
                    role: ComponentRole::Service,
                    completion: None,
                    implementation: ImplementationIdentity::try_new("not-installed/service", "1")?,
                    configuration_version: 1,
                    configuration: BTreeMap::new(),
                    dependencies: BTreeMap::new(),
                },
            );
            "unavailable"
        } else {
            native(&mut changed, mode, &directory.join("native"), 0, 0)?;
            config.plugins = changed.plugins.clone();
            "native"
        };
        let expected = changed.desired.clone();
        let mut host = HostProcess::spawn(&config, &directory)?;
        let ready = host.wait(|status| status.ready)?;
        assert!(ready.configuration_persistent);
        let mut duplicate = HostProcess::spawn(&config, &directory)?;
        assert!(!duplicate
            .wait_exit()
            .with_context(|| format!("{mode}: duplicate host must be rejected"))?
            .success());
        assert_eq!(host.status()?.generation, ready.generation);
        let apply = host.operation(ControlCommand::Apply {
            expected_revision: ready.configuration_revision.unwrap(),
            management_request_id: format!("persist-{mode}"),
            desired: Box::new(changed.desired),
        })?;
        let applied = host.wait_operation(apply)?;
        assert_eq!(
            applied.phase,
            OperationPhase::Completed,
            "{mode}: {applied:?}"
        );
        assert_eq!(applied.result.as_ref().unwrap()["durable"], true);
        let revision = applied.result.as_ref().unwrap()["revision"]
            .as_u64()
            .unwrap();
        host.stop()
            .with_context(|| format!("{mode}: stop after apply; status={:?}", host.status()))?;
        let mut restored = HostProcess::spawn(&config, &directory)?;
        let observed = restored.wait(|status| {
            status.configuration_revision == Some(revision)
                && if mode == "missing-factory" {
                    status.phase == HostPhase::Running
                        && status
                            .components
                            .iter()
                            .any(|node| node.id == component && node.realization == "Blocked")
                } else {
                    status.phase == HostPhase::Failed
                }
        })?;
        assert!(!observed.ready, "{mode}");
        let state = observed
            .components
            .iter()
            .find(|node| node.id == component)
            .context("persisted component observation")?;
        if mode == "missing-factory" {
            assert!(observed.healthy);
            assert!(state.failure.is_none());
            assert_eq!(state.lifecycle, "Stopped");
        } else {
            assert!(!observed.healthy);
            assert!(state.failure.is_some(), "{mode}: {state:?}");
        }
        let ControlReply::Configuration { configuration } = restored
            .client()?
            .request(Uuid::new_v4(), ControlCommand::Configuration)?
        else {
            panic!("configuration response")
        };
        assert_eq!(configuration.desired, expected, "{mode}");
        if mode == "missing-factory" {
            let reconcile = restored.operation(ControlCommand::Reconcile)?;
            let reconciled = restored.wait_operation(reconcile)?;
            assert_eq!(
                reconciled.phase,
                OperationPhase::Completed,
                "{mode}: {reconciled:?}"
            );
            assert!(!restored.status()?.ready);
        }
        restored.stop().with_context(|| {
            format!(
                "{mode}: stop after reconstruction; status={:?}",
                restored.status()
            )
        })?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_embedded_host_retains_independent_child_reaping() -> Result<()> {
    let directory = Directory::new()?;
    let mut config = HostConfig::empty("embedded", directory.join("a"));
    let child_config = HostConfig::empty("child", directory.join("b"));
    child(&mut config, "child-owner", &child_config)?;
    let credentials = Credentials::new(&config)?;
    let client = ControlClient::new(credentials.clone());
    let task = tokio::spawn(drasi_instance_host::run(
        config,
        credentials,
        PathBuf::from(env!("CARGO_BIN_EXE_drasi-instance-host")),
    ));
    let deadline = Instant::now() + WATCHDOG;
    let mut pid = None;
    while Instant::now() < deadline {
        if let Ok(ControlReply::Status { observation }) = client
            .request_async(Uuid::new_v4(), ControlCommand::Status)
            .await
        {
            if observation.ready {
                pid = observation.children[0].pid;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let pid = pid.context("embedded host did not start its real child")?;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let observation_path = std::fs::read_dir(directory.join("a/children"))?
        .filter_map(|entry| entry.ok())
        .find(|entry| entry.path().is_dir())
        .context("supervisor observation directory")?
        .path()
        .join("observation.json");
    while Instant::now() < deadline {
        if let Ok(bytes) = std::fs::read(&observation_path) {
            if let Ok(observed) =
                serde_json::from_slice::<drasi_instance_host::ChildObservation>(&bytes)
            {
                if observed.reaped && observed.phase == ChildPhase::Terminated {
                    assert_eq!(observed.pid, Some(pid));
                    gone(pid)?;
                    use std::os::fd::AsRawFd;
                    let lock = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(directory.join("a/owner.lock"))?;
                    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0
                    {
                        return Ok(());
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    anyhow::bail!("embedded cancellation did not reap its child and release the parent namespace")
}

fn standard_plugin(implementation: &str) -> Result<(&'static str, PathBuf)> {
    let package = match implementation {
        "drasi.standard/volatile-counter" => "drasi-computation-source-counter",
        "drasi.standard/middleware" => "drasi-computation-transformer-middleware",
        "drasi.standard/arithmetic" => "drasi-computation-transformer-arithmetic",
        "drasi.standard/capture" => "drasi-computation-sink-capture",
        _ => anyhow::bail!("unknown native test implementation {implementation}"),
    };
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_drasi-instance-host"));
    let directory = std::env::var_os("DRASI_NATIVE_PLUGIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| executable.parent().unwrap().to_owned());
    let path = directory.join(format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        package.replace('-', "_"),
        std::env::consts::DLL_SUFFIX
    ));
    ensure!(
        path.is_file(),
        "build the native component library: cargo build -p {package} --features dynamic-plugin"
    );
    Ok((package, path))
}

fn standard(
    config: &mut HostConfig,
    name: &str,
    implementation: &str,
    role: ComponentRole,
    values: serde_json::Value,
) -> Result<()> {
    let (package, plugin) = standard_plugin(implementation)?;
    if !config.plugins.iter().any(|value| value.path == plugin) {
        config.plugins.push(NativePlugin {
            path: plugin,
            sha256: None,
        });
    }
    let mut identity = ImplementationIdentity::try_new(implementation, "1")?;
    identity.plugin = Some(PluginIdentity {
        id: package.into(),
        version: env!("CARGO_PKG_VERSION").into(),
    });
    let mut ports = Vec::new();
    if role != ComponentRole::Source {
        ports.push(PortDescriptor::new(
            PortId::try_new("in")?,
            PortDirection::Input,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        ));
    }
    if role != ComponentRole::Sink {
        ports.push(PortDescriptor::new(
            PortId::try_new("out")?,
            PortDirection::Output,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        ));
    }
    add(
        config,
        ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(id(name), ports)?.with_plugin_identity(
                identity
                    .plugin
                    .clone()
                    .expect("component plugin provenance"),
            )?,
            role,
            completion: (role == ComponentRole::Sink).then_some(SinkCompletion::Handled),
            implementation: identity,
            configuration_version: 1,
            configuration: values
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| {
                    (
                        key.as_str().into(),
                        ConfigurationValue::Literal(value.clone()),
                    )
                })
                .collect(),
            dependencies: BTreeMap::new(),
        },
    );
    if let Some(stream) = values.get("stream").and_then(|value| value.as_str()) {
        config
            .desired
            .topology
            .components
            .last_mut()
            .unwrap()
            .streams
            .insert(PortId::try_new("out")?, StreamId::try_new(stream)?);
    }
    Ok(())
}

fn connect(config: &mut HostConfig, producer: &str, consumer: &str) -> Result<()> {
    config
        .desired
        .topology
        .relationships
        .push(DesiredRelationship {
            definition: EdgeDefinition::new(
                Endpoint::new(id(producer), PortId::try_new("out")?),
                Endpoint::new(id(consumer), PortId::try_new("in")?),
            ),
            policy: RelationshipPolicy::default(),
            pipe: DesiredPipe::Bounded { capacity: 8 },
        });
    Ok(())
}

fn bridge(
    config: &mut HostConfig,
    value: &drasi_computation_bridge_common::BridgeConfig,
    role: ComponentRole,
) -> Result<()> {
    let mut specification = value.specification(role)?;
    let control = ResourceId::try_new(format!("{}-control", specification.descriptor.id()))?;
    specification
        .dependencies
        .insert("control".into(), vec![control.clone()]);
    config
        .desired
        .topology
        .resources
        .push(ResourceSpecification {
            id: control.clone(),
            role: ResourceRole::Component,
            ownership: ResourceOwnership::Graph,
            binding: control.as_str().into(),
        });
    config.desired.topology.resource_configurations.insert(control,
        json!({"kind": if role == ComponentRole::Source { "bridgeIngressControl" } else { "bridgeEgressControl" }}));
    add(config, specification);
    if role == ComponentRole::Source {
        config
            .desired
            .topology
            .components
            .last_mut()
            .unwrap()
            .streams
            .insert(
                PortId::try_new("out")?,
                StreamId::try_new(value.stream.as_str())?,
            );
    }
    Ok(())
}

#[test]
fn real_bidirectional_native_data_drains_through_native_cuts_before_process_exit() -> Result<()> {
    use drasi_computation_bridge_common::{
        BridgeConfig, Limits, Namespace, PortableSchema, Profile, Transport,
    };
    let directory = Directory::new()?;
    let mut parent_config = HostConfig::empty("parent", directory.join("a"));
    let mut child_config = HostConfig::empty("child", directory.join("b"));
    let outgoing = BridgeConfig {
        link_id: "input".into(),
        sender_instance: "parent".into(),
        receiver_instance: "child".into(),
        sender_endpoint: "to-child".into(),
        receiver_endpoint: "from-parent".into(),
        stream: "p/input".into(),
        peer_stream: "c/input".into(),
        schema: PortableSchema::GraphChange,
        profile: Profile::Volatile,
        failure_scope: drasi_core::interface::FailureMode::ProcessRestart,
        namespace: Namespace {
            version: 1,
            source_prefixes: vec!["parent/".into()],
            query_prefixes: Vec::new(),
        },
        transport: Transport::Unix {
            data_path: directory.join("in.d").to_string_lossy().into_owned(),
            control_path: directory.join("in.c").to_string_lossy().into_owned(),
        },
        limits: Limits::default(),
    };
    let mut incoming = outgoing.clone();
    std::mem::swap(&mut incoming.stream, &mut incoming.peer_stream);
    let returning = BridgeConfig {
        link_id: "output".into(),
        sender_instance: "child".into(),
        receiver_instance: "parent".into(),
        sender_endpoint: "to-parent".into(),
        receiver_endpoint: "from-child".into(),
        stream: "c/output".into(),
        peer_stream: "p/output".into(),
        transport: Transport::Unix {
            data_path: directory.join("out.d").to_string_lossy().into_owned(),
            control_path: directory.join("out.c").to_string_lossy().into_owned(),
        },
        ..outgoing.clone()
    };
    let mut returned = returning.clone();
    std::mem::swap(&mut returned.stream, &mut returned.peer_stream);
    standard(
        &mut parent_config,
        "parent/input",
        "drasi.standard/volatile-counter",
        ComponentRole::Source,
        json!({"stream":"p/input","count":64,"interval_ms":20}),
    )?;
    parent_config
        .desired
        .topology
        .components
        .last_mut()
        .unwrap()
        .lifecycle
        .auto_start = false;
    let output = directory.join("results");
    standard(
        &mut parent_config,
        "parent/capture",
        "drasi.standard/capture",
        ComponentRole::Sink,
        json!({"path":output}),
    )?;
    bridge(&mut parent_config, &outgoing, ComponentRole::Sink)?;
    bridge(&mut parent_config, &returned, ComponentRole::Source)?;
    bridge(&mut child_config, &incoming, ComponentRole::Source)?;
    bridge(&mut child_config, &returning, ComponentRole::Sink)?;
    standard(
        &mut child_config,
        "child/arithmetic",
        "drasi.standard/arithmetic",
        ComponentRole::Transformer,
        json!({"stream":"c/output","add":10}),
    )?;
    connect(&mut parent_config, "parent/input", "to-child")?;
    connect(&mut parent_config, "from-child", "parent/capture")?;
    connect(&mut child_config, "from-parent", "child/arithmetic")?;
    connect(&mut child_config, "child/arithmetic", "to-parent")?;
    child_config.stop_stages = vec![StopStage::Quiesce {
        components: vec![id("from-parent"), id("child/arithmetic"), id("to-parent")],
    }];
    child(&mut parent_config, "child-owner", &child_config)?;
    parent_config.stop_stages = vec![
        StopStage::Quiesce {
            components: vec![id("parent/input"), id("to-child")],
        },
        StopStage::StopChild {
            component: id("child-owner"),
        },
        StopStage::Quiesce {
            components: vec![id("from-child"), id("parent/capture")],
        },
    ];
    let mut parent = HostProcess::spawn(&parent_config, &directory)?;
    let initial = parent.wait(|status| status.ready)?;
    let start = parent.operation(ControlCommand::StartComponents {
        components: vec![id("parent/input")],
    })?;
    assert_eq!(
        parent.wait_operation(start)?.phase,
        OperationPhase::Completed
    );
    let deadline = Instant::now() + WATCHDOG;
    while Instant::now() < deadline {
        if std::fs::read_to_string(&output)
            .unwrap_or_default()
            .lines()
            .count()
            >= 8
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    ensure!(
        std::fs::read_to_string(&output)?.lines().count() < 64,
        "the stop request must overlap live bidirectional traffic"
    );
    let stop = parent.operation(ControlCommand::StopTree)?;
    let completed = parent.wait_operation(stop)?;
    assert_eq!(completed.phase, OperationPhase::Completed, "{completed:?}");
    ensure!(
        parent.wait_exit()?.success(),
        "host did not complete graceful process exit"
    );
    let final_observation: HostObservation = serde_json::from_slice(&std::fs::read(
        parent_config.state_dir.join("last-observation.json"),
    )?)?;
    let expected = final_observation.stop_cuts["egress:to-child"].client_sequence;
    assert!((8..=64).contains(&expected));
    assert_eq!(
        final_observation.stop_cuts["ingress:from-child"].client_sequence,
        expected
    );
    let codec = FactoryRegistry::standard()
        .envelope_codec(std::num::NonZeroUsize::new(1024 * 1024).unwrap())?;
    let mut values = Vec::new();
    for line in std::fs::read_to_string(&output)?.lines() {
        let envelope = codec.decode(line.as_bytes())?;
        for change in GraphChangeCodec::decode_changes(&envelope)? {
            let drasi_core::models::SourceChange::Insert {
                element:
                    drasi_core::models::Element::Node {
                        metadata,
                        properties,
                    },
            } = change
            else {
                panic!("expected graph insertion")
            };
            assert_eq!(metadata.reference.source_id.as_ref(), "parent/input");
            let drasi_core::models::ElementValue::Integer(value) = properties.get("value").unwrap()
            else {
                panic!("expected arithmetic value")
            };
            values.push(*value);
        }
    }
    assert_eq!(values, (10..10 + expected as i64).collect::<Vec<i64>>());
    let child = &initial.children[0];
    gone(child.pid.unwrap())?;
    Ok(())
}

#[test]
fn an_empty_bridge_queue_without_a_peer_cut_cannot_claim_graceful_stop() -> Result<()> {
    use drasi_computation_bridge_common::{
        BridgeConfig, BridgeEndpointControl, Limits, Namespace, PortableSchema, Profile, Transport,
    };
    let directory = Directory::new()?;
    let mut config = HostConfig::empty("receiver", directory.join("a"));
    let incoming = BridgeConfig {
        link_id: "inlet".into(),
        sender_instance: "external".into(),
        receiver_instance: "receiver".into(),
        sender_endpoint: "outlet".into(),
        receiver_endpoint: "inlet".into(),
        stream: "receiver/in".into(),
        peer_stream: "external/out".into(),
        schema: PortableSchema::GraphChange,
        profile: Profile::Volatile,
        failure_scope: drasi_core::interface::FailureMode::ProcessRestart,
        namespace: Namespace {
            version: 1,
            source_prefixes: vec!["external/".into()],
            query_prefixes: Vec::new(),
        },
        transport: Transport::Unix {
            data_path: directory.join("in.d").to_string_lossy().into_owned(),
            control_path: directory.join("in.c").to_string_lossy().into_owned(),
        },
        limits: Limits::default(),
    };
    bridge(&mut config, &incoming, ComponentRole::Source)?;
    standard(
        &mut config,
        "capture",
        "drasi.standard/capture",
        ComponentRole::Sink,
        json!({"path":directory.join("output")}),
    )?;
    connect(&mut config, "inlet", "capture")?;
    config.stop_stages = vec![StopStage::Quiesce {
        components: vec![id("inlet"), id("capture")],
    }];
    let mut host = HostProcess::spawn(&config, &directory)?;
    host.wait(|status| status.ready)?;
    let attempt = host.operation(ControlCommand::StopTree)?;
    let refused = host.wait_operation(attempt)?;
    assert_eq!(refused.phase, OperationPhase::Failed);
    assert!(refused
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("no peer-sealed cut"));
    assert!(host
        .status()?
        .components
        .iter()
        .any(|component| component.id == "inlet" && component.lifecycle == "Running"));

    let mut sender = incoming;
    std::mem::swap(&mut sender.stream, &mut sender.peer_stream);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let mut endpoint =
                BridgeEndpointControl::connect(sender, ComponentRole::Sink, None).await?;
            let cut = endpoint.seal(Duration::from_secs(5)).await?;
            ensure!(cut.client_sequence == 0, "unexpected admitted input");
            Ok::<_, anyhow::Error>(())
        })?;
    host.stop()
}
