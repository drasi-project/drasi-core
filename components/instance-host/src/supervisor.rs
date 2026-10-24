// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Child, ChildStderr, ChildStdout, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_lib::computation::v1::*;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch};
use uuid::Uuid;

use crate::{
    config::{HostConfig, RuntimeBudget},
    platform,
    protocol::{ControlClient, ControlCommand, ControlReply, Credentials, OperationPhase},
    state, HostObservation, SUPERVISOR_IMPLEMENTATION,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildPhase {
    Defined,
    Launching,
    Ready,
    Stopping,
    Stopped,
    Failed,
    CleanupRequired,
    Terminated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildIntent {
    Running,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildObservation {
    pub component: String,
    pub instance_id: String,
    pub manifest_fingerprint: String,
    pub generation: Option<Uuid>,
    pub pid: Option<u32>,
    pub intent: ChildIntent,
    pub phase: ChildPhase,
    pub reaped: bool,
    #[serde(default)]
    pub ownership_lost: bool,
    #[serde(default)]
    pub exit_verified: bool,
    pub last_known: Option<Box<HostObservation>>,
    pub last_observed_ms: u64,
    pub stale: bool,
    pub error: Option<String>,
    pub diagnostics: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Launch {
    pub credentials: Credentials,
    pub owner_instance: String,
}

type Reply = oneshot::Sender<std::result::Result<ChildObservation, String>>;

enum Action {
    Start,
    Stop,
    Force,
    Repair,
    ConfirmPreviousExit,
}

enum Request {
    Register {
        component: String,
        config: Box<HostConfig>,
        reply: oneshot::Sender<Result<Registration>>,
    },
    Action {
        component: String,
        registration: Option<Uuid>,
        generation: Option<Uuid>,
        action: Action,
        reply: Reply,
    },
}

#[derive(Clone)]
struct Registration {
    component: String,
    token: Uuid,
    observed: watch::Receiver<ChildObservation>,
}

struct Process {
    child: Child,
    credentials: Credentials,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    force_requested: bool,
    stop_operation: Option<Uuid>,
    handshaken: bool,
}

struct Slot {
    registration: Uuid,
    config: HostConfig,
    directory: PathBuf,
    observed: watch::Sender<ChildObservation>,
    process: Option<Process>,
    pending: Vec<Reply>,
    deadline: Option<Instant>,
    next_health: Instant,
}

struct Shared {
    observations: Mutex<BTreeMap<String, ChildObservation>>,
    leases: Mutex<Vec<Arc<dyn std::any::Any + Send + Sync>>>,
    closing: AtomicBool,
    finished: AtomicBool,
}

struct Owner {
    requests: mpsc::SyncSender<Request>,
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::Release);
        // The worker retains every Child until wait/try_wait actually reaps it.
        // If the OS cannot finish a kill promptly, it remains a cleanup owner.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.shared.finished.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if self.shared.finished.load(Ordering::Acquire) {
            if let Some(thread) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = thread.join();
            }
        }
    }
}

#[derive(Clone)]
pub struct ChildSupervisor(Arc<Owner>);

impl ChildSupervisor {
    pub(crate) fn request_emergency_cleanup(&self) {
        self.0.shared.closing.store(true, Ordering::Release);
    }

    pub(crate) fn retain_parent_lease<T: Send + Sync + 'static>(&self, lease: Arc<T>) {
        self.0
            .shared
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(lease);
    }

    /// The executable is a host-selected capability, not a manifest-supplied
    /// shell command. The generic CLI supplies current_exe at every level.
    pub fn new(
        executable: PathBuf,
        state_dir: PathBuf,
        instance_id: String,
        budget: RuntimeBudget,
    ) -> Result<Self> {
        budget.validate()?;
        ensure!(
            executable.is_absolute() && executable.is_file(),
            "host executable is unavailable"
        );
        let directory = state_dir.join("children");
        let lease = Arc::new(state::StateDirectory::acquire(&directory, &instance_id)?);
        let shared = Arc::new(Shared {
            observations: Mutex::new(BTreeMap::new()),
            leases: Mutex::new(vec![lease]),
            closing: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        });
        let (send, receive) = mpsc::sync_channel(64);
        let worker_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("drasi-child-owner".into())
            .spawn(move || {
                let mut worker = Worker {
                    executable,
                    directory,
                    instance_id,
                    budget,
                    slots: BTreeMap::new(),
                    shared: worker_shared,
                };
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run(&receive))).is_err() {
                    worker.shared.closing.store(true, Ordering::Release);
                    for slot in worker.slots.values_mut() {
                        set_error(slot, "child owner thread panicked; emergency instance termination is required");
                    }
                    worker.run(&receive);
                }
            })?;
        Ok(Self(Arc::new(Owner {
            requests: send,
            shared,
            thread: Mutex::new(Some(thread)),
        })))
    }

    pub fn observations(&self) -> Vec<ChildObservation> {
        self.0
            .shared
            .observations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    async fn register(&self, component: String, config: HostConfig) -> Result<Registration> {
        let (reply, receive) = oneshot::channel();
        self.0
            .requests
            .try_send(Request::Register {
                component,
                config: Box::new(config),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("child owner queue is full or closed"))?;
        receive
            .await
            .context("child owner stopped during registration")?
    }

    async fn action(
        &self,
        component: &str,
        registration: Option<Uuid>,
        generation: Option<Uuid>,
        action: Action,
    ) -> Result<ChildObservation> {
        ensure!(
            !self.0.shared.closing.load(Ordering::Acquire),
            "child owner is shutting down"
        );
        let (reply, receive) = oneshot::channel();
        self.0
            .requests
            .try_send(Request::Action {
                component: component.to_owned(),
                registration,
                generation,
                action,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("child owner queue is full or closed"))?;
        receive
            .await
            .context("child owner stopped while an operation was pending")?
            .map_err(anyhow::Error::msg)
    }

    pub async fn stop(&self, component: &str, generation: Uuid) -> Result<ChildObservation> {
        self.action(component, None, Some(generation), Action::Stop)
            .await
    }

    pub async fn force(&self, component: &str, generation: Uuid) -> Result<ChildObservation> {
        self.action(component, None, Some(generation), Action::Force)
            .await
    }

    pub async fn repair(&self, component: &str, generation: Uuid) -> Result<ChildObservation> {
        self.action(component, None, Some(generation), Action::Repair)
            .await
    }

    pub(crate) async fn confirm_previous_exit(
        &self,
        component: &str,
        generation: Uuid,
    ) -> Result<ChildObservation> {
        self.action(
            component,
            None,
            Some(generation),
            Action::ConfirmPreviousExit,
        )
        .await
    }

    pub async fn stop_all(&self) -> Result<()> {
        for child in self.observations() {
            if let Some(generation) = child.generation {
                self.stop(&child.component, generation).await?;
            }
        }
        Ok(())
    }

    pub async fn close(&self) -> Result<()> {
        ensure!(
            self.observations()
                .iter()
                .all(|child| child.reaped || child.exit_verified),
            "live child ownership remains; explicitly stop or force each instance"
        );
        self.0.shared.closing.store(true, Ordering::Release);
        while !self.0.shared.finished.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if let Some(thread) = self
            .0
            .thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("child owner thread failed"))?;
        }
        Ok(())
    }
}

struct Worker {
    executable: PathBuf,
    directory: PathBuf,
    instance_id: String,
    budget: RuntimeBudget,
    slots: BTreeMap<String, Slot>,
    shared: Arc<Shared>,
}

impl Worker {
    fn run(&mut self, requests: &mpsc::Receiver<Request>) {
        loop {
            let closing = self.shared.closing.load(Ordering::Acquire);
            if closing {
                for slot in self.slots.values_mut() {
                    if let Some(process) = slot.process.as_mut() {
                        if !process.force_requested {
                            match platform::kill_owned(&mut process.child) {
                                Ok(()) => process.force_requested = true,
                                Err(error) => set_error(
                                    slot,
                                    &format!("emergency child termination: {error:#}"),
                                ),
                            }
                        }
                    }
                }
            } else {
                match requests.recv_timeout(Duration::from_millis(10)) {
                    Ok(request) => self.handle(request),
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        self.shared.closing.store(true, Ordering::Release)
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
            for slot in self.slots.values_mut() {
                Self::poll_slot(&self.budget, slot);
                self.shared
                    .observations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        slot.observed.borrow().component.clone(),
                        slot.observed.borrow().clone(),
                    );
            }
            if closing {
                if self.slots.values().all(|slot| slot.process.is_none()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.shared.finished.store(true, Ordering::Release);
    }

    fn handle(&mut self, request: Request) {
        match request {
            Request::Register {
                component,
                config,
                reply,
            } => {
                let result = self.register(component, *config);
                let _ = reply.send(result);
            }
            Request::Action {
                component,
                registration,
                generation,
                action,
                reply,
            } => {
                let Some(slot) = self.slots.get_mut(&component) else {
                    let _ = reply.send(Err("child is not locally owned".into()));
                    return;
                };
                if registration.is_some_and(|token| token != slot.registration)
                    || generation.is_some_and(|generation| {
                        slot.observed.borrow().generation != Some(generation)
                    })
                {
                    let _ =
                        reply.send(Err("stale child registration or process generation".into()));
                    return;
                }
                if slot.pending.len() >= 8 {
                    let _ = reply.send(Err("child lifecycle waiter budget exceeded".into()));
                    return;
                }
                let result = match action {
                    Action::Start => Self::start(&self.executable, &self.instance_id, slot, false),
                    Action::Repair => Self::start(&self.executable, &self.instance_id, slot, true),
                    Action::Stop => Self::stop(&self.budget, slot),
                    Action::Force => Self::force(&self.budget, slot),
                    Action::ConfirmPreviousExit => Self::confirm_previous_exit(slot),
                };
                match result {
                    Ok(true) => {
                        let _ = reply.send(Ok(slot.observed.borrow().clone()));
                    }
                    Ok(false) => slot.pending.push(reply),
                    Err(error) => {
                        set_error(slot, &format!("{error:#}"));
                        let _ = reply.send(Err(format!("{error:#}")));
                    }
                }
            }
        }
    }

    fn register(&mut self, component: String, config: HostConfig) -> Result<Registration> {
        config.validate()?;
        ensure!(
            config.instance_id != self.instance_id
                && Some(config.state_dir.as_path()) != self.directory.parent(),
            "an owned child must have a distinct instance and state namespace"
        );
        ComponentId::try_new(component.clone())?;
        let fingerprint = config.fingerprint()?;
        let previous = self.slots.get(&component);
        ensure!(
            previous.is_none_or(|slot| slot.process.is_none()),
            "previous child generation is still owned"
        );
        ensure!(
            previous.is_some() || self.slots.len() < self.budget.max_owned_children,
            "owned-child budget exceeded"
        );
        for (id, slot) in &self.slots {
            ensure!(
                id == &component
                    || (slot.config.state_dir != config.state_dir
                        && slot.config.instance_id != config.instance_id),
                "another owned child has this instance identity or state namespace"
            );
        }
        let key = crate::config::digest(component.as_bytes());
        let directory = self.directory.join(key);
        state::private_directory(&directory)?;
        let path = directory.join("observation.json");
        let mut observation = ChildObservation {
            component: component.clone(),
            instance_id: config.instance_id.clone(),
            manifest_fingerprint: fingerprint.clone(),
            generation: None,
            pid: None,
            intent: ChildIntent::Running,
            phase: ChildPhase::Defined,
            reaped: true,
            ownership_lost: false,
            exit_verified: true,
            last_known: None,
            last_observed_ms: now_ms(),
            stale: true,
            error: None,
            diagnostics: String::new(),
        };
        if let Some(previous) = previous {
            observation = previous.observed.borrow().clone();
        } else if path.exists() {
            observation = state::read_json(&path, 2 * crate::config::MAX_MANIFEST_BYTES)?;
            observation.stale = true;
            if !observation.reaped {
                observation.phase = ChildPhase::Failed;
                observation.error = Some(
                    "previous owner ended without joined cleanup; explicit repair is required"
                        .into(),
                );
                observation.ownership_lost = true;
                observation.exit_verified = false;
            }
        }
        if observation.manifest_fingerprint != fingerprint {
            if observation.generation.is_none() {
                observation.intent = ChildIntent::Running;
                observation.phase = ChildPhase::Defined;
                observation.error = None;
            } else if observation.intent == ChildIntent::Running && !observation.ownership_lost {
                observation.phase = ChildPhase::Defined;
                observation.error = None;
            }
        }
        ensure!(
            observation.component == component,
            "supervisor state belongs to another component"
        );
        observation.instance_id = config.instance_id.clone();
        observation.manifest_fingerprint = fingerprint;
        state::atomic_json(&directory.join("manifest.json"), &config)?;
        state::atomic_json(&path, &observation)?;
        let (observed, receive) = watch::channel(observation);
        let registration = Uuid::new_v4();
        self.slots.insert(
            component.clone(),
            Slot {
                registration,
                config,
                directory,
                observed,
                process: None,
                pending: Vec::new(),
                deadline: None,
                next_health: Instant::now(),
            },
        );
        Ok(Registration {
            component,
            token: registration,
            observed: receive,
        })
    }

    fn start(executable: &PathBuf, owner: &str, slot: &mut Slot, repair: bool) -> Result<bool> {
        if slot.process.is_some() {
            ensure!(
                !repair,
                "cannot repair until the old process has exited and been reaped"
            );
            return Ok(slot.observed.borrow().phase == ChildPhase::Ready);
        }
        if !repair {
            if slot.observed.borrow().intent == ChildIntent::Stopped {
                return Ok(true);
            }
            ensure!(
                slot.observed.borrow().phase != ChildPhase::Failed,
                "terminal child failure requires explicit repair"
            );
        }
        if slot.observed.borrow().ownership_lost {
            ensure!(repair, "lost process ownership needs explicit repair");
            Self::confirm_previous_exit(slot)?;
        }
        let credentials = Credentials::new(&slot.config)?;
        slot.observed.send_modify(|observed| {
            observed.generation = Some(credentials.generation);
            observed.intent = ChildIntent::Running;
            observed.phase = ChildPhase::Launching;
            observed.reaped = false;
            observed.ownership_lost = false;
            observed.exit_verified = false;
            observed.pid = None;
            observed.stale = true;
            observed.error = None;
            observed.diagnostics.clear();
        });
        // Persist/reserve ownership before an executable can run.
        persist(slot)?;
        let child = Command::new(executable)
            .arg("--config")
            .arg(slot.directory.join("manifest.json"))
            .arg("--owned")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                slot.observed.send_modify(|value| {
                    value.reaped = true;
                    value.exit_verified = true;
                    value.phase = ChildPhase::Failed;
                });
                return Err(error.into());
            }
        };
        slot.process = Some(Process {
            child,
            credentials: credentials.clone(),
            stdout: None,
            stderr: None,
            force_requested: false,
            stop_operation: None,
            handshaken: false,
        });
        let process = slot.process.as_mut().expect("reserved process");
        process.stdout = process.child.stdout.take();
        process.stderr = process.child.stderr.take();
        slot.observed
            .send_modify(|value| value.pid = Some(process.child.id()));
        // Even a bootstrap write or pipe setup failure retains this Child.
        if let Some(stdout) = &process.stdout {
            platform::nonblocking(stdout)?;
        }
        if let Some(stderr) = &process.stderr {
            platform::nonblocking(stderr)?;
        }
        crate::protocol::write_frame(
            process
                .child
                .stdin
                .as_mut()
                .context("missing child ownership pipe")?,
            &Launch {
                credentials,
                owner_instance: owner.into(),
            },
            8192,
        )?;
        process
            .child
            .stdin
            .as_mut()
            .expect("ownership pipe")
            .flush()?;
        slot.deadline =
            Some(Instant::now() + Duration::from_millis(slot.config.runtime.startup_timeout_ms));
        slot.next_health = Instant::now();
        persist(slot)?;
        Ok(false)
    }

    fn stop(budget: &RuntimeBudget, slot: &mut Slot) -> Result<bool> {
        slot.observed.send_modify(|value| {
            value.intent = ChildIntent::Stopped;
        });
        persist(slot)?;
        let Some(process) = slot.process.as_mut() else {
            ensure!(!slot.observed.borrow().ownership_lost || slot.observed.borrow().exit_verified,
                "old process ownership was lost; explicit repair must verify exit before cleanup can complete");
            slot.observed.send_modify(|value| {
                value.phase = ChildPhase::Stopped;
            });
            persist(slot)?;
            return Ok(true);
        };
        if process.force_requested {
            return Ok(false);
        }
        let operation = match process.stop_operation {
            Some(previous) => {
                match client(process, budget).request(
                    Uuid::new_v4(),
                    ControlCommand::Operation {
                        operation_id: previous,
                    },
                ) {
                    Ok(ControlReply::Operation { operation })
                        if operation.phase == OperationPhase::Failed =>
                    {
                        Uuid::new_v4()
                    }
                    // An unknown or incomplete attempt can still be running.
                    // Retry its same id rather than overlap a retained drain.
                    _ => previous,
                }
            }
            None => Uuid::new_v4(),
        };
        process.stop_operation = Some(operation);
        slot.observed
            .send_modify(|value| value.phase = ChildPhase::Stopping);
        slot.deadline =
            Some(Instant::now() + Duration::from_millis(slot.config.runtime.operation_timeout_ms));
        let reply = client(process, budget).request(operation, ControlCommand::StopTree)?;
        ensure!(
            matches!(reply, ControlReply::Accepted { operation_id } if operation_id == operation),
            "child did not accept the stop operation"
        );
        persist(slot)?;
        Ok(false)
    }

    fn force(budget: &RuntimeBudget, slot: &mut Slot) -> Result<bool> {
        slot.observed.send_modify(|value| {
            value.intent = ChildIntent::Stopped;
            value.phase = ChildPhase::Stopping;
        });
        persist(slot)?;
        let Some(process) = slot.process.as_mut() else {
            ensure!(slot.observed.borrow().reaped || slot.observed.borrow().exit_verified,
                "cannot force an unowned old PID; process-generation ownership must be re-established");
            return Ok(true);
        };
        platform::kill_owned(&mut process.child)?;
        process.force_requested = true;
        slot.deadline = Some(Instant::now() + Duration::from_millis(budget.operation_timeout_ms));
        Ok(false)
    }

    fn confirm_previous_exit(slot: &mut Slot) -> Result<bool> {
        ensure!(
            slot.process.is_none(),
            "a live owned process must be stopped and reaped, not reconstructed"
        );
        if slot.observed.borrow().ownership_lost {
            if let Some(pid) = slot.observed.borrow().pid {
                ensure!(platform::process_absent(pid)?, "prior process PID is still present; no replacement or stale-PID termination is permitted");
            }
            let _state =
                state::StateDirectory::acquire(&slot.config.state_dir, &slot.config.instance_id)?;
            slot.observed
                .send_modify(|value| value.exit_verified = true);
            persist(slot)?;
        }
        Ok(true)
    }

    fn poll_slot(budget: &RuntimeBudget, slot: &mut Slot) {
        let Some(process) = slot.process.as_mut() else {
            return;
        };
        let mut diagnostics = Vec::new();
        drain(
            &mut process.stdout,
            &mut diagnostics,
            budget.diagnostic_bytes,
        );
        drain(
            &mut process.stderr,
            &mut diagnostics,
            budget.diagnostic_bytes,
        );
        if !diagnostics.is_empty() {
            slot.observed.send_modify(|value| {
                let mut bytes = value.diagnostics.as_bytes().to_vec();
                bytes.extend_from_slice(&diagnostics);
                let start = bytes.len().saturating_sub(budget.diagnostic_bytes);
                value.diagnostics = bounded(
                    &String::from_utf8_lossy(&bytes[start..]),
                    budget.diagnostic_bytes,
                );
            });
        }
        match process.child.try_wait() {
            Ok(Some(status)) => {
                let final_observation = state::read_json::<HostObservation>(
                    &slot.config.state_dir.join("last-observation.json"),
                    2 * crate::config::MAX_MANIFEST_BYTES,
                )
                .ok()
                .filter(|value| {
                    value.generation == process.credentials.generation
                        && value.instance_id == slot.config.instance_id
                        && value.manifest_fingerprint == process.credentials.manifest_fingerprint
                });
                let expected = process.stop_operation.is_some()
                    && status.success()
                    && final_observation
                        .as_ref()
                        .is_some_and(|value| value.phase == crate::HostPhase::Stopped);
                let forced = process.force_requested;
                slot.observed.send_modify(|value| {
                    if let Some(observation) = final_observation {
                        value.last_known = Some(Box::new(observation));
                    }
                    value.reaped = true;
                    value.exit_verified = true;
                    value.stale = true;
                    value.last_observed_ms = now_ms();
                    value.phase = if forced { ChildPhase::Terminated } else if expected { ChildPhase::Stopped } else { ChildPhase::Failed };
                    if !expected && !forced { value.error = Some(format!("child exited before graceful completion: {status}")); }
                    if forced { value.error = Some("explicit instance termination; prior data/commit outcomes remain unresolved".into()); }
                });
                slot.process = None;
                slot.deadline = None;
                if let Err(error) = persist(slot) {
                    set_error(slot, &format!("persist child exit: {error:#}"));
                }
                finish(slot, expected || forced);
                return;
            }
            Ok(None) => {}
            Err(error) => {
                set_error(slot, &format!("wait for child: {error}"));
                return;
            }
        }
        if Instant::now() >= slot.next_health && !process.force_requested {
            slot.next_health = Instant::now() + Duration::from_millis(budget.health_interval_ms);
            let result = (|| {
                let client = client(process, budget);
                if !process.handshaken {
                    let ControlReply::Hello {
                        runtime,
                        package_version,
                        capabilities,
                    } = client.request(Uuid::new_v4(), ControlCommand::Hello)?
                    else {
                        anyhow::bail!("child did not negotiate host capabilities");
                    };
                    ensure!(
                        runtime == "drasi-lib"
                            && package_version == env!("CARGO_PKG_VERSION")
                            && capabilities
                                .iter()
                                .any(|value| value == "native-management-v1")
                            && capabilities
                                .iter()
                                .any(|value| value == "ordered-composition-stop-v1"),
                        "child does not run the compatible normal DrasiLib host"
                    );
                    process.handshaken = true;
                }
                client.status()
            })();
            match result {
                Ok(observation) => {
                    if observation.manifest_fingerprint
                        != slot.observed.borrow().manifest_fingerprint
                        || observation.generation != process.credentials.generation
                    {
                        set_error(
                            slot,
                            "child readiness identity does not match the owned launch",
                        );
                    } else {
                        let ready = observation.ready && observation.healthy;
                        let failed = matches!(
                            observation.phase,
                            crate::HostPhase::Failed | crate::HostPhase::CleanupRequired
                        );
                        let error = observation.error.clone();
                        slot.observed.send_modify(|value| {
                            value.last_observed_ms = now_ms();
                            value.stale = false;
                            value.last_known = Some(Box::new(observation));
                            if value.phase == ChildPhase::Launching && ready {
                                value.phase = ChildPhase::Ready;
                            }
                            if failed && value.phase != ChildPhase::Stopping {
                                value.phase = ChildPhase::Failed;
                                value.error = error;
                            }
                        });
                        if slot.observed.borrow().phase == ChildPhase::Ready
                            && slot.deadline.is_some()
                        {
                            slot.deadline = None;
                            if let Err(error) = persist(slot) {
                                set_error(slot, &format!("persist child readiness: {error:#}"));
                            }
                            finish(slot, true);
                        } else if failed {
                            let _ = persist(slot);
                            finish(slot, false);
                        }
                    }
                }
                Err(error) => {
                    slot.observed.send_modify(|value| {
                        value.stale = true;
                        if value.phase != ChildPhase::Launching {
                            value.error = Some(bounded(&format!("child control unavailable; process is not proven dead: {error:#}"), budget.diagnostic_bytes));
                            if value.phase == ChildPhase::Ready { value.phase = ChildPhase::CleanupRequired; }
                        }
                    });
                }
            }
        }
        if slot
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            slot.deadline = None;
            slot.observed.send_modify(|value| {
                value.phase = ChildPhase::CleanupRequired;
                value.error = Some("child lifecycle deadline elapsed; ownership, control and data paths remain retained".into());
            });
            let _ = persist(slot);
            finish(slot, false);
        }
    }
}

fn client(process: &Process, budget: &RuntimeBudget) -> ControlClient {
    ControlClient {
        credentials: process.credentials.clone(),
        max_frame_bytes: budget.max_control_frame_bytes,
        timeout: Duration::from_millis(budget.peer_timeout_ms),
    }
}

fn drain<T: Read>(reader: &mut Option<T>, output: &mut Vec<u8>, limit: usize) {
    let Some(reader) = reader else { return };
    let mut buffer = [0_u8; 1024];
    while output.len() < limit {
        let count = (limit - output.len()).min(buffer.len());
        match reader.read(&mut buffer[..count]) {
            Ok(0) => return,
            Ok(count) => output.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

fn persist(slot: &Slot) -> Result<()> {
    state::atomic_json(
        &slot.directory.join("observation.json"),
        &*slot.observed.borrow(),
    )
}

fn set_error(slot: &mut Slot, error: &str) {
    slot.observed.send_modify(|value| {
        value.error = Some(bounded(error, 4096));
        value.phase = if slot.process.is_some() {
            ChildPhase::CleanupRequired
        } else {
            ChildPhase::Failed
        };
    });
    let _ = persist(slot);
}

fn finish(slot: &mut Slot, success: bool) {
    let observation = slot.observed.borrow().clone();
    for reply in slot.pending.drain(..) {
        let result = if success {
            Ok(observation.clone())
        } else {
            Err(observation
                .error
                .clone()
                .unwrap_or_else(|| "child is not healthy".into()))
        };
        let _ = reply.send(result);
    }
}

pub(crate) fn bounded(value: &str, bytes: usize) -> String {
    let mut end = value.len().min(bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub struct SupervisorFactory {
    descriptor: FactoryDescriptor,
    supervisor: Option<ChildSupervisor>,
}

/// An explicitly granted, process-local hosting capability, never serialized
/// into a child or passed over the native plugin ABI.
pub struct ChildSupervisorResource(pub ChildSupervisor);

impl ChildSupervisorResource {
    pub fn handle(supervisor: ChildSupervisor) -> ResourceHandle {
        let owner = Arc::new(Self(supervisor));
        ResourceHandle::new(ResourceRole::Component, owner.clone()).with_cleanup(owner)
    }
}

#[async_trait]
impl ResourceCleanup for ChildSupervisorResource {
    async fn shutdown(&self) -> Result<()> {
        self.0.stop_all().await?;
        self.0.close().await
    }
}

impl Default for SupervisorFactory {
    fn default() -> Self {
        Self::configured(None)
    }
}

impl SupervisorFactory {
    pub fn new(supervisor: ChildSupervisor) -> Self {
        Self::configured(Some(supervisor))
    }

    fn configured(supervisor: Option<ChildSupervisor>) -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new(SUPERVISOR_IMPLEMENTATION, "1")
                    .expect("constant identity"),
                role: ComponentRole::Service,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::from([(
                        "manifest".into(),
                        ConfigurationField {
                            value_type: ConfigurationType::Object,
                            required: true,
                            secret: false,
                        },
                    )]),
                    allow_additional: false,
                },
                dependencies: BTreeMap::from([(
                    "owner".into(),
                    ResourceRequirement {
                        role: ResourceRole::Component,
                        minimum: 0,
                        maximum: Some(1),
                        concrete_type: Some(std::any::TypeId::of::<ChildSupervisorResource>()),
                    },
                )]),
            },
            supervisor,
        }
    }

    pub fn specification(id: ComponentId, manifest: &HostConfig) -> Result<ComponentSpecification> {
        manifest.validate()?;
        Ok(ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(id, Vec::new())?,
            role: ComponentRole::Service,
            completion: None,
            implementation: ImplementationIdentity::try_new(SUPERVISOR_IMPLEMENTATION, "1")?,
            configuration_version: 1,
            configuration: BTreeMap::from([(
                "manifest".into(),
                ConfigurationValue::Literal(serde_json::to_value(manifest)?),
            )]),
            dependencies: BTreeMap::new(),
        })
    }
}

#[async_trait]
impl ComponentFactory for SupervisorFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn validate(&self, specification: &ComponentSpecification) -> Result<()> {
        ensure!(
            specification.descriptor.ports().is_empty(),
            "a child supervisor is portless process control, never a data transformer"
        );
        ensure!(
            self.supervisor.is_some()
                || specification
                    .dependencies
                    .get("owner")
                    .is_some_and(|ids| ids.len() == 1),
            "the embedding host must provide a ChildSupervisorResource in the owner dependency"
        );
        let Some(ConfigurationValue::Literal(value)) = specification.configuration.get("manifest")
        else {
            anyhow::bail!("a child requires a literal host manifest");
        };
        serde_json::from_value::<HostConfig>(value.clone())?.validate()
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let create = async {
            let configuration = context
                .configuration()
                .get("manifest")
                .context("missing child manifest")?
                .clone();
            let manifest: HostConfig = serde_json::from_value(configuration.clone())?;
            let supervisor = if context.specification.dependencies.contains_key("owner") {
                context
                    .resources::<ChildSupervisorResource>("owner")?
                    .first()
                    .context("supervisor owner resource is unavailable")?
                    .0
                    .clone()
            } else {
                self.supervisor
                    .clone()
                    .context("supervisor owner resource is unavailable")?
            };
            let registration = supervisor
                .register(context.component_id.to_string(), manifest)
                .await?;
            Ok::<_, anyhow::Error>(ConstructedComponent::service(Box::new(SupervisorService {
                descriptor: context.specification.descriptor.clone(),
                configuration: serde_json::json!({"manifest": configuration}),
                supervisor,
                registration,
                control: None,
            })))
        };
        create.await.map_err(ComponentCreationError::terminal)
    }
}

struct SupervisorService {
    descriptor: ComponentDescriptor,
    configuration: serde_json::Value,
    supervisor: ChildSupervisor,
    registration: Registration,
    control: Option<ComponentControl>,
}

#[async_trait]
impl ComputationComponent for SupervisorService {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }
    fn requires_readiness_confirmation(&self) -> bool {
        true
    }
    fn bind_control(&mut self, control: ComponentControl) {
        self.control = Some(control);
    }

    async fn start(&mut self) -> Result<()> {
        self.supervisor
            .action(
                &self.registration.component,
                Some(self.registration.token),
                None,
                Action::Start,
            )
            .await?;
        if let Some(control) = &self.control {
            control.ready()?;
        }
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if self.supervisor.0.shared.closing.load(Ordering::Acquire) {
            let mut observed = self.registration.observed.clone();
            observed
                .wait_for(|child| child.reaped || child.exit_verified)
                .await
                .context("child owner closed before termination was established")?;
            return Ok(());
        }
        self.supervisor
            .action(
                &self.registration.component,
                Some(self.registration.token),
                None,
                Action::Stop,
            )
            .await?;
        Ok(())
    }

    async fn deprovision(&mut self) -> Result<()> {
        self.stop().await
    }
}

#[async_trait]
impl ComputationService for SupervisorService {
    async fn run(&mut self) -> Result<()> {
        loop {
            let value = self.registration.observed.borrow_and_update().clone();
            if matches!(
                value.phase,
                ChildPhase::Failed | ChildPhase::CleanupRequired
            ) {
                anyhow::bail!(
                    "{}",
                    value.error.as_deref().unwrap_or("child is not healthy")
                );
            }
            self.registration
                .observed
                .changed()
                .await
                .context("child supervisor stopped")?;
        }
    }
}
