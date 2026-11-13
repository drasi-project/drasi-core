// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Immutable container-creation receipts; these do not confirm completed cleanup.

use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};
#[cfg(unix)]
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Component,
};
use uuid::Uuid;

const OWNER_ENV: &str = "DRASI_TEST_OWNER_TOKEN";
const DIRECTORY_ENV: &str = "DRASI_TEST_RECEIPT_DIR";

pub struct Fixture {
    name: &'static str,
    image: String,
    source: &'static str,
    max_attempts: u32,
}

impl Fixture {
    pub fn new(
        name: &'static str,
        image: impl Into<String>,
        source: &'static str,
        max_attempts: u32,
    ) -> Self {
        Self {
            name,
            image: image.into(),
            source,
            max_attempts,
        }
    }
}

pub struct Attribution {
    directory: PathBuf,
    directory_identity: (u64, u64, u32),
    pub(crate) owner_token: String,
    pub(crate) fixture_id: Uuid,
    executable: String,
    actor_pid: u32,
    test_name: Option<String>,
    fixture: Fixture,
}

impl Attribution {
    pub fn from_env(fixture: Fixture) -> Result<Option<Self>> {
        Self::from_settings(
            fixture,
            (OWNER_ENV, DIRECTORY_ENV),
            std::env::var_os(OWNER_ENV),
            std::env::var_os(DIRECTORY_ENV),
        )
    }

    pub(crate) fn from_settings(
        fixture: Fixture,
        environment: (&str, &str),
        owner: Option<OsString>,
        directory: Option<OsString>,
    ) -> Result<Option<Self>> {
        let (owner_env, directory_env) = environment;
        let (owner, directory) = match (owner, directory) {
            (None, None) => return Ok(None),
            (Some(owner), Some(directory)) => (owner, directory),
            _ => anyhow::bail!("{owner_env} and {directory_env} must be set together"),
        };
        ensure!(
            !fixture.name.is_empty()
                && fixture
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && !fixture.image.is_empty()
                && !fixture.image.chars().any(char::is_control)
                && !fixture.source.is_empty()
                && fixture.max_attempts > 0,
            "invalid fixture attribution metadata"
        );
        let owner_token = owner
            .into_string()
            .map_err(|_| anyhow::anyhow!("{owner_env} must be Unicode"))?;
        let token = Uuid::parse_str(&owner_token).context("fixture owner token must be a UUID")?;
        ensure!(
            !token.is_nil() && token.to_string() == owner_token,
            "fixture owner token must be a non-nil canonical UUID"
        );
        let directory = PathBuf::from(
            directory
                .into_string()
                .map_err(|_| anyhow::anyhow!("{directory_env} must be Unicode"))?,
        );
        let directory_identity = directory_identity(&directory)?;
        let executable = std::env::current_exe()
            .context("fixture attribution executable")?
            .canonicalize()
            .context("canonical fixture attribution executable")?
            .into_os_string()
            .into_string()
            .map_err(|_| anyhow::anyhow!("fixture attribution executable must be Unicode"))?;
        Ok(Some(Self {
            directory,
            directory_identity,
            owner_token,
            fixture_id: Uuid::new_v4(),
            executable,
            actor_pid: std::process::id(),
            test_name: std::thread::current().name().map(str::to_owned),
            fixture,
        }))
    }

    pub fn container_name(&self, attempt: u32) -> String {
        format!(
            "drasi-{}-{}-{}-a{attempt}",
            self.fixture.name, self.owner_token, self.fixture_id
        )
    }

    pub fn labels(&self, attempt: u32) -> BTreeMap<&'static str, String> {
        BTreeMap::from([
            ("drasi.test.owner", self.owner_token.clone()),
            ("drasi.test.fixture", self.fixture_id.to_string()),
            ("drasi.test.attempt", attempt.to_string()),
        ])
    }

    pub fn record_intent(&self, attempt: u32) -> Result<()> {
        self.record(attempt, "intent", None)
    }

    pub(crate) async fn before_creation<T, F, Fut>(
        &self,
        attempt: u32,
        observer: Option<&ObserverAdmission>,
        create: F,
    ) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        self.record_intent(attempt)?;
        if let Some(observer) = observer {
            observer.wait(self, attempt).await?;
        }
        create().await
    }

    pub fn record_started(&self, attempt: u32, container_id: &str) -> Result<()> {
        ensure!(
            container_id.len() == 64
                && container_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "fixture container identity must be a full canonical engine ID"
        );
        self.record(attempt, "started", Some(container_id))
    }

    fn record(&self, attempt: u32, phase: &str, container_id: Option<&str>) -> Result<()> {
        ensure!(
            (1..=self.fixture.max_attempts).contains(&attempt),
            "invalid fixture attempt"
        );
        self.write_receipt(
            &self
                .directory
                .join(format!("{}-a{attempt}.{phase}.json", self.fixture_id)),
            &self.receipt(attempt, phase, container_id),
        )
    }

    fn receipt(&self, attempt: u32, phase: &str, container_id: Option<&str>) -> Value {
        let mut receipt = json!({
            "schema_version": 1,
            "phase": if phase == "intent" { "creation_intent" } else { "started" },
            "owner_token": self.owner_token,
            "fixture_id": self.fixture_id.to_string(),
            "attempt": attempt,
            "container_name": self.container_name(attempt),
            "labels": self.labels(attempt),
            "image": self.fixture.image,
            "fixture_source": self.fixture.source,
            "executable": self.executable,
            "actor_pid": self.actor_pid,
            "test_name": self.test_name,
        });
        if let Some(container_id) = container_id {
            receipt["container_id"] = json!(container_id);
        }
        receipt
    }

    #[cfg(unix)]
    fn write_receipt(&self, path: &Path, receipt: &Value) -> Result<()> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        ensure!(
            directory_identity(&self.directory)? == self.directory_identity,
            "fixture receipt directory identity changed"
        );
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("create new fixture receipt {}", path.display()))?;
        let metadata = file.metadata().context("fixture receipt file metadata")?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == self.directory_identity.2
                && metadata.permissions().mode() & 0o777 == 0o600,
            "fixture receipt directory must be actor-owned and receipts owner-only 0600"
        );
        ensure!(
            directory_identity(&self.directory)? == self.directory_identity,
            "fixture receipt directory identity changed during receipt creation"
        );
        let directory = File::open(&self.directory).context("open fixture receipt directory")?;
        file.write_all(&serde_json::to_vec(receipt)?)
            .context("write fixture receipt")?;
        file.write_all(b"\n").context("terminate fixture receipt")?;
        file.sync_all().context("fsync fixture receipt")?;
        directory
            .sync_all()
            .context("fsync fixture receipt directory")?;
        ensure!(
            directory_identity(&self.directory)? == self.directory_identity,
            "fixture receipt directory identity changed during receipt persistence"
        );
        Ok(())
    }

    #[cfg(not(unix))]
    fn write_receipt(&self, _: &Path, _: &Value) -> Result<()> {
        anyhow::bail!("fixture attribution requires Unix ownership verification")
    }
}

#[cfg(unix)]
pub(crate) fn directory_identity(path: &Path) -> Result<(u64, u64, u32)> {
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};

    ensure!(
        path.is_absolute(),
        "fixture receipt directory must be absolute"
    );
    ensure!(
        !path
            .as_os_str()
            .as_bytes()
            .split(|byte| *byte == b'/')
            .any(|part| part == b"." || part == b".."),
        "fixture receipt directory must not contain traversal components"
    );
    let mut current = PathBuf::new();
    for component in path.components() {
        ensure!(
            matches!(component, Component::RootDir | Component::Normal(_)),
            "invalid fixture receipt directory component"
        );
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("inspect fixture receipt directory {}", current.display()))?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "fixture receipt directory traversal must contain only real directories"
        );
    }
    let metadata = fs::symlink_metadata(path).context("fixture receipt directory metadata")?;
    ensure!(
        metadata.mode() & 0o077 == 0,
        "fixture receipt directory must be private to its owner"
    );
    Ok((metadata.dev(), metadata.ino(), metadata.uid()))
}

#[cfg(not(unix))]
pub(crate) fn directory_identity(_: &Path) -> Result<(u64, u64, u32)> {
    anyhow::bail!("fixture attribution requires Unix ownership verification")
}

pub(crate) struct ObserverAdmission {
    #[cfg(unix)]
    state: observer_admission::State,
}

impl ObserverAdmission {
    pub(crate) fn from_settings(
        owner: Option<&Attribution>,
        directory: Option<OsString>,
        timeout_ms: Option<OsString>,
        expectations_sha256: Option<OsString>,
    ) -> Result<Option<Self>> {
        let (directory, timeout_ms, expectations_sha256) =
            match (directory, timeout_ms, expectations_sha256) {
                (None, None, None) => return Ok(None),
                (Some(directory), Some(timeout), Some(digest)) => (directory, timeout, digest),
                _ => anyhow::bail!(
                    "DRASI_REDIS_TEST_ACK_DIR, DRASI_REDIS_TEST_ACK_TIMEOUT_MS and \
                     DRASI_REDIS_TEST_ACK_EXPECTATIONS_SHA256 must be set together"
                ),
            };
        let owner = owner.context("Observer admission requires enabled fixture attribution")?;
        let directory = directory
            .into_string()
            .map_err(|_| anyhow::anyhow!("Observer acknowledgement directory must be Unicode"))?;
        let timeout_ms = timeout_ms
            .into_string()
            .map_err(|_| anyhow::anyhow!("Observer timeout must be Unicode"))?;
        let timeout: u64 = timeout_ms
            .parse()
            .context("Observer timeout must be an integer")?;
        ensure!(
            (1..=10_000).contains(&timeout) && timeout.to_string() == timeout_ms,
            "Observer timeout must be canonical positive milliseconds, at most 10000"
        );
        let digest = expectations_sha256
            .into_string()
            .map_err(|_| anyhow::anyhow!("Observer expectations digest must be Unicode"))?;
        ensure!(
            canonical_sha256(&digest),
            "Observer expectations digest must be canonical lowercase SHA256"
        );
        #[cfg(unix)]
        {
            Ok(Some(Self {
                state: observer_admission::State::new(
                    owner,
                    PathBuf::from(directory),
                    timeout,
                    &digest,
                )?,
            }))
        }
        #[cfg(not(unix))]
        {
            let _ = (owner, directory, timeout, digest);
            anyhow::bail!("Observer admission requires Unix ownership verification")
        }
    }

    async fn wait(&self, owner: &Attribution, attempt: u32) -> Result<()> {
        #[cfg(unix)]
        {
            self.state.wait(owner, attempt).await
        }
        #[cfg(not(unix))]
        {
            let _ = (owner, attempt);
            anyhow::bail!("Observer admission requires Unix ownership verification")
        }
    }
}

fn canonical_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(unix)]
mod observer_admission {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{fs::Metadata, io::Read, os::unix::fs::MetadataExt, time::Duration};
    use tokio::time::{Instant, MissedTickBehavior};

    const MAX_RECORD_BYTES: u64 = 65_536;

    fn exact_keys(value: &Value, keys: &[&str]) -> Result<()> {
        let object = value
            .as_object()
            .context("Observer record must be an object")?;
        ensure!(
            object.len() == keys.len() && keys.iter().all(|key| object.contains_key(*key)),
            "Observer record has missing or unexpected fields"
        );
        Ok(())
    }

    fn sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn file_identity(metadata: &Metadata) -> (u64, u64, u32, u32, u64) {
        (
            metadata.dev(),
            metadata.ino(),
            metadata.uid(),
            metadata.mode(),
            metadata.nlink(),
        )
    }

    fn directory_record(path: &Path) -> Result<Value> {
        let (dev, ino, uid) = directory_identity(path)?;
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.mode() & 0o7777 == 0o700,
            "Observer admission directories must be owner-only 0700"
        );
        Ok(json!({"dev": dev, "ino": ino, "uid": uid, "mode": 0o700}))
    }

    struct PinnedRecord {
        path: PathBuf,
        identity: (u64, u64, u32, u32, u64),
        bytes: Vec<u8>,
    }

    impl PinnedRecord {
        fn read(path: PathBuf, uid: u64) -> Result<Self> {
            let before = fs::symlink_metadata(&path)
                .with_context(|| format!("inspect Observer admission file {}", path.display()))?;
            ensure!(
                before.is_file()
                    && !before.file_type().is_symlink()
                    && u64::from(before.uid()) == uid
                    && before.mode() & 0o7777 == 0o600
                    && before.nlink() == 1
                    && before.len() <= MAX_RECORD_BYTES,
                "Observer admission files must be actor-owned regular single-link 0600 files"
            );
            let mut file = File::open(&path)?;
            let identity = file_identity(&before);
            ensure!(
                file_identity(&file.metadata()?) == identity,
                "Observer admission file identity changed while opening"
            );
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(MAX_RECORD_BYTES + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= MAX_RECORD_BYTES
                    && file_identity(&file.metadata()?) == identity
                    && file_identity(&fs::symlink_metadata(&path)?) == identity,
                "Observer admission file changed while reading"
            );
            Ok(Self {
                path,
                identity,
                bytes,
            })
        }

        fn unchanged(&self) -> Result<()> {
            let current = Self::read(self.path.clone(), u64::from(self.identity.2))?;
            ensure!(
                current.identity == self.identity && current.bytes == self.bytes,
                "immutable Observer admission file changed"
            );
            Ok(())
        }

        fn json(&self) -> Result<Value> {
            serde_json::from_slice(&self.bytes).context("invalid Observer admission JSON")
        }
    }

    fn executable_digest(path: &str) -> Result<String> {
        let path = Path::new(path);
        ensure!(
            path.is_absolute() && path.canonicalize()? == path,
            "Observer executable expectation must be a canonical absolute path"
        );
        let before = fs::symlink_metadata(path)?;
        ensure!(
            before.is_file(),
            "Observer executable must be a regular file"
        );
        let mut file = File::open(path)?;
        ensure!(
            file_identity(&file.metadata()?) == file_identity(&before),
            "Observer executable changed while opening"
        );
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 65_536];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        let after = file.metadata()?;
        ensure!(
            file_identity(&after) == file_identity(&before)
                && after.len() == before.len()
                && after.mtime() == before.mtime()
                && after.mtime_nsec() == before.mtime_nsec()
                && after.ctime() == before.ctime()
                && after.ctime_nsec() == before.ctime_nsec()
                && file_identity(&fs::symlink_metadata(path)?) == file_identity(&before),
            "Observer executable changed while hashing"
        );
        Ok(format!("{:x}", digest.finalize()))
    }

    fn actor_argv() -> Result<Value> {
        let argv = std::env::args_os()
            .map(|argument| {
                argument
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("Observer actor argv must be Unicode"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(json!(argv))
    }

    fn expected_command(argv: &Value) -> Result<String> {
        let arguments = argv.as_array().context("pinned expected argv array")?;
        let arguments = arguments
            .iter()
            .map(|argument| argument.as_str().context("pinned Unicode argument"))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            !arguments.is_empty()
                && arguments.iter().all(|argument| {
                    !argument.is_empty()
                        && !argument.chars().any(|character| {
                            character.is_whitespace()
                                || character.is_control()
                                || matches!(character, '\'' | '"' | '\\')
                        })
                }),
            "pinned argv has an ambiguous Observer command representation"
        );
        let command = arguments.join(" ");
        ensure!(
            !command.is_empty()
                && command.trim() == command
                && command.len() < 4096
                && !command.chars().any(char::is_control),
            "pinned argv has no supported untruncated Observer command representation"
        );
        Ok(command)
    }

    pub(super) struct State {
        directory: PathBuf,
        receipt_identity: Value,
        ack_identity: Value,
        manifest: PinnedRecord,
        expected_executable_sha256: String,
        expected_argv: Value,
        expected_command: String,
        fixture_id: Uuid,
        timeout: Duration,
    }

    impl State {
        pub(super) fn new(
            owner: &Attribution,
            directory: PathBuf,
            timeout_ms: u64,
            manifest_sha256: &str,
        ) -> Result<Self> {
            let receipt_identity = directory_record(&owner.directory)?;
            let ack_identity = directory_record(&directory)?;
            ensure!(
                directory_identity(&owner.directory)? == owner.directory_identity
                    && receipt_identity != ack_identity
                    && !directory.starts_with(&owner.directory)
                    && !owner.directory.starts_with(&directory)
                    && receipt_identity["uid"] == ack_identity["uid"],
                "Observer and receipt directories must be separate, unchanged and actor-owned"
            );
            let manifest = PinnedRecord::read(
                directory.join("expectations.json"),
                ack_identity["uid"].as_u64().context("ack directory uid")?,
            )?;
            ensure!(
                sha256(&manifest.bytes) == manifest_sha256,
                "Observer expectations do not match the prelaunch digest"
            );
            let value = manifest.json()?;
            exact_keys(
                &value,
                &[
                    "schema_version",
                    "owner_token",
                    "receipt_dir_identity",
                    "ack_dir_identity",
                    "executables",
                ],
            )?;
            ensure!(
                value["schema_version"] == 1
                    && value["owner_token"] == owner.owner_token
                    && value["receipt_dir_identity"] == receipt_identity
                    && value["ack_dir_identity"] == ack_identity,
                "Observer expectations do not match this job and its directories"
            );
            let argv = actor_argv()?;
            let mut selected = None;
            for entry in value["executables"]
                .as_array()
                .context("expected executables array")?
            {
                exact_keys(entry, &["executable", "executable_sha256", "actor_argv"])?;
                let executable = entry["executable"]
                    .as_str()
                    .context("expected executable")?;
                let digest = entry["executable_sha256"]
                    .as_str()
                    .context("expected binary SHA256")?;
                let arguments = entry["actor_argv"]
                    .as_array()
                    .context("expected argv array")?;
                ensure!(
                    Path::new(executable).is_absolute()
                        && canonical_sha256(digest)
                        && !arguments.is_empty()
                        && arguments.iter().all(Value::is_string),
                    "invalid prelaunch executable expectation"
                );
                if executable == owner.executable && entry["actor_argv"] == argv {
                    ensure!(selected.is_none(), "ambiguous prelaunch actor expectation");
                    selected = Some(digest.to_owned());
                }
            }
            let expected_executable_sha256 =
                selected.context("actor absent from prelaunch expectations")?;
            let expected_command = expected_command(&argv)?;
            ensure!(
                executable_digest(&owner.executable)? == expected_executable_sha256,
                "actor binary differs from the prelaunch executable expectation"
            );
            let state = Self {
                directory,
                receipt_identity,
                ack_identity,
                manifest,
                expected_executable_sha256,
                expected_argv: argv,
                expected_command,
                fixture_id: owner.fixture_id,
                timeout: Duration::from_millis(timeout_ms),
            };
            state.anchors(owner)?;
            Ok(state)
        }

        fn anchors(&self, owner: &Attribution) -> Result<()> {
            ensure!(
                owner.fixture_id == self.fixture_id
                    && directory_record(&owner.directory)? == self.receipt_identity
                    && directory_record(&self.directory)? == self.ack_identity,
                "Observer admission fixture or directory identity changed"
            );
            self.manifest.unchanged()
        }

        fn validate_ack(
            &self,
            owner: &Attribution,
            attempt: u32,
            intent: &PinnedRecord,
            ack: &PinnedRecord,
        ) -> Result<()> {
            let value = ack.json()?;
            exact_keys(
                &value,
                &[
                    "schema_version",
                    "owner_token",
                    "fixture_id",
                    "attempt",
                    "intent_sha256",
                    "actor_pid",
                    "actor_birth",
                    "actor_command",
                    "executable",
                    "executable_sha256",
                    "receipt_dir_identity",
                    "ack_dir_identity",
                ],
            )?;
            ensure!(
                value["schema_version"] == 1
                    && value["owner_token"] == owner.owner_token
                    && value["fixture_id"] == owner.fixture_id.to_string()
                    && value["attempt"] == attempt
                    && value["intent_sha256"] == sha256(&intent.bytes)
                    && value["actor_pid"] == owner.actor_pid
                    && value["actor_command"] == self.expected_command
                    && value["executable"] == owner.executable
                    && value["executable_sha256"] == self.expected_executable_sha256
                    && value["receipt_dir_identity"] == self.receipt_identity
                    && value["ack_dir_identity"] == self.ack_identity,
                "Observer acknowledgement does not match the immutable intent and prelaunch actor"
            );
            let birth = value["actor_birth"]
                .as_str()
                .context("Observer actor birth")?;
            ensure!(
                chrono::NaiveDateTime::parse_from_str(birth, "%a %b %e %T %Y").is_ok(),
                "Observer acknowledgement lacks a valid OS-observed actor birth"
            );
            ensure!(
                actor_argv()? == self.expected_argv
                    && executable_digest(&owner.executable)? == self.expected_executable_sha256,
                "actor argv or binary changed after prelaunch validation"
            );
            self.anchors(owner)?;
            intent.unchanged()?;
            ack.unchanged()?;
            self.anchors(owner)
        }

        pub(super) async fn wait(&self, owner: &Attribution, attempt: u32) -> Result<()> {
            let deadline = Instant::now() + self.timeout;
            let admission = async {
                self.anchors(owner)?;
                let intent = PinnedRecord::read(
                    owner
                        .directory
                        .join(format!("{}-a{attempt}.intent.json", owner.fixture_id)),
                    self.receipt_identity["uid"]
                        .as_u64()
                        .context("receipt directory uid")?,
                )?;
                let mut expected_intent =
                    serde_json::to_vec(&owner.receipt(attempt, "intent", None))?;
                expected_intent.push(b'\n');
                ensure!(
                    intent.bytes == expected_intent,
                    "Observer admission requires this exact persisted creation intent"
                );
                let path = self
                    .directory
                    .join(format!("{}-a{attempt}.observed.json", owner.fixture_id));
                let period = Duration::from_millis(50);
                let mut polling = tokio::time::interval_at(Instant::now() + period, period);
                polling.set_missed_tick_behavior(MissedTickBehavior::Delay);
                loop {
                    self.anchors(owner)?;
                    intent.unchanged()?;
                    match fs::symlink_metadata(&path) {
                        Ok(_) => {
                            let ack = PinnedRecord::read(
                                path.clone(),
                                self.ack_identity["uid"]
                                    .as_u64()
                                    .context("ack directory uid")?,
                            )?;
                            self.validate_ack(owner, attempt, &intent, &ack)?;
                            ensure!(
                                Instant::now() < deadline,
                                "Observer acknowledgement validation exceeded its admission bound"
                            );
                            return Ok(());
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(error).context("inspect Observer acknowledgement")
                        }
                    }
                    polling.tick().await;
                }
            };
            tokio::time::timeout_at(deadline, admission)
                .await
                .context("Observer acknowledgement timeout before engine creation")?
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{
            cell::Cell,
            os::unix::{
                ffi::OsStringExt,
                fs::{symlink, OpenOptionsExt, PermissionsExt},
            },
        };

        struct PrivateDirectory(PathBuf);

        impl PrivateDirectory {
            fn new() -> Result<Self> {
                let path = std::env::temp_dir()
                    .canonicalize()?
                    .join(format!("drasi-observer-negative-{}", Uuid::new_v4()));
                fs::create_dir(&path)?;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
                Ok(Self(path))
            }
        }

        impl Drop for PrivateDirectory {
            fn drop(&mut self) {
                fs::remove_dir_all(&self.0).expect("remove this test's private directory");
            }
        }

        fn write_control(path: &Path, bytes: &[u8]) -> Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(path)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            File::open(path.parent().context("control-file parent")?)?.sync_all()?;
            Ok(())
        }

        // Synthetic files below are rejection tests, never genuine-Observer positive evidence.
        struct NegativeFixture {
            receipts: PrivateDirectory,
            acks: PrivateDirectory,
            owner: Attribution,
            digest: String,
        }

        impl NegativeFixture {
            fn new() -> Result<Self> {
                let receipts = PrivateDirectory::new()?;
                let acks = PrivateDirectory::new()?;
                let owner = Attribution::from_settings(
                    Fixture::new(
                        "redis",
                        "redis:7-alpine",
                        "shared-tests/src/redis_helpers.rs",
                        5,
                    ),
                    ("owner", "receipts"),
                    Some(Uuid::new_v4().to_string().into()),
                    Some(receipts.0.clone().into_os_string()),
                )?
                .context("negative fixture attribution")?;
                let manifest = json!({
                    "schema_version": 1,
                    "owner_token": owner.owner_token,
                    "receipt_dir_identity": directory_record(&receipts.0)?,
                    "ack_dir_identity": directory_record(&acks.0)?,
                    "executables": [{
                        "executable": owner.executable,
                        "executable_sha256": executable_digest(&owner.executable)?,
                        "actor_argv": actor_argv()?,
                    }],
                });
                let bytes = serde_json::to_vec(&manifest)?;
                let digest = sha256(&bytes);
                write_control(&acks.0.join("expectations.json"), &bytes)?;
                Ok(Self {
                    receipts,
                    acks,
                    owner,
                    digest,
                })
            }

            fn gate(&self, timeout_ms: &str) -> Result<ObserverAdmission> {
                ObserverAdmission::from_settings(
                    Some(&self.owner),
                    Some(self.acks.0.clone().into_os_string()),
                    Some(timeout_ms.into()),
                    Some(self.digest.clone().into()),
                )?
                .context("negative-fixture admission")
            }

            fn replace_manifest(&mut self, value: &Value) -> Result<()> {
                let bytes = serde_json::to_vec(value)?;
                let mut file = OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(self.acks.0.join("expectations.json"))?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                self.digest = sha256(&bytes);
                Ok(())
            }

            fn ack_path(&self) -> PathBuf {
                self.acks
                    .0
                    .join(format!("{}-a1.observed.json", self.owner.fixture_id))
            }

            fn negative_ack(&self) -> Result<Value> {
                let mut bytes = serde_json::to_vec(&self.owner.receipt(1, "intent", None))?;
                bytes.push(b'\n');
                Ok(json!({
                    "schema_version": 1,
                    "owner_token": self.owner.owner_token,
                    "fixture_id": self.owner.fixture_id.to_string(),
                    "attempt": 1,
                    "intent_sha256": sha256(&bytes),
                    "actor_pid": self.owner.actor_pid,
                    "actor_birth": "Thu Jan  1 00:00:00 1970",
                    "actor_command": expected_command(&actor_argv()?)?,
                    "executable": self.owner.executable,
                    "executable_sha256": executable_digest(&self.owner.executable)?,
                    "receipt_dir_identity": directory_record(&self.receipts.0)?,
                    "ack_dir_identity": directory_record(&self.acks.0)?,
                }))
            }

            async fn assert_zero_creation(&self, gate: &ObserverAdmission) -> Result<()> {
                let calls = Cell::new(0);
                let result = self
                    .owner
                    .before_creation(1, Some(gate), || async {
                        calls.set(calls.get() + 1);
                        Ok(())
                    })
                    .await;
                assert!(result.is_err(), "negative fixture must fail admission");
                assert_eq!(calls.get(), 0, "engine callback must remain unreachable");
                assert!(!self
                    .receipts
                    .0
                    .join(format!("{}-a1.started.json", self.owner.fixture_id))
                    .exists());
                Ok(())
            }
        }

        #[test]
        fn observer_settings_default_off_and_reject_every_partial_opt_in() -> Result<()> {
            assert!(ObserverAdmission::from_settings(None, None, None, None)?.is_none());
            for mask in 1..8 {
                assert!(ObserverAdmission::from_settings(
                    None,
                    (mask & 1 != 0).then(|| OsString::from("/missing")),
                    (mask & 2 != 0).then(|| OsString::from("10000")),
                    (mask & 4 != 0).then(|| OsString::from("a".repeat(64))),
                )
                .is_err());
            }
            let fixture = NegativeFixture::new()?;
            for timeout in ["", "0", "-1", "+1", "01", "10001", "1.5", " 1"] {
                assert!(fixture.gate(timeout).is_err());
            }
            for digest in [
                "short".to_owned(),
                "A".repeat(64),
                "g".repeat(64),
                "0".repeat(64),
            ] {
                assert!(ObserverAdmission::from_settings(
                    Some(&fixture.owner),
                    Some(fixture.acks.0.clone().into_os_string()),
                    Some("10000".into()),
                    Some(digest.into()),
                )
                .is_err());
            }
            for setting in 0..3 {
                let mut values = [
                    fixture.acks.0.clone().into_os_string(),
                    OsString::from("10000"),
                    OsString::from(fixture.digest.clone()),
                ];
                values[setting] = OsString::from_vec(vec![0xff]);
                let [directory, timeout, digest] = values;
                assert!(ObserverAdmission::from_settings(
                    Some(&fixture.owner),
                    Some(directory),
                    Some(timeout),
                    Some(digest),
                )
                .is_err());
            }
            assert_eq!(fs::read_dir(&fixture.receipts.0)?.count(), 0);
            Ok(())
        }

        #[tokio::test]
        async fn observer_rejects_malformed_stale_and_wrong_identity_before_engine() -> Result<()> {
            let mutations = [
                ("schema_version", json!(2)),
                ("owner_token", json!(Uuid::new_v4().to_string())),
                ("fixture_id", json!(Uuid::new_v4().to_string())),
                ("attempt", json!(2)),
                ("intent_sha256", json!("0".repeat(64))),
                ("actor_pid", json!(0)),
                ("actor_birth", json!("not-an-OS-birth")),
                ("actor_command", json!("wrong-command")),
                ("executable", json!("/wrong/executable")),
                ("executable_sha256", json!("0".repeat(64))),
                ("receipt_dir_identity", json!({})),
                ("ack_dir_identity", json!({})),
            ];
            for (field, value) in mutations {
                let fixture = NegativeFixture::new()?;
                let gate = fixture.gate("10000")?;
                let mut ack = fixture.negative_ack()?;
                ack[field] = value;
                write_control(&fixture.ack_path(), &serde_json::to_vec(&ack)?)?;
                fixture.assert_zero_creation(&gate).await?;
            }
            for bytes in [b"{".to_vec(), b"{}".to_vec()] {
                let fixture = NegativeFixture::new()?;
                let gate = fixture.gate("10000")?;
                write_control(&fixture.ack_path(), &bytes)?;
                fixture.assert_zero_creation(&gate).await?;
            }
            Ok(())
        }

        #[tokio::test]
        async fn observer_rejects_symlink_hardlink_and_nonprivate_ack_before_engine() -> Result<()>
        {
            for violation in ["symlink", "hardlink", "permissions"] {
                let fixture = NegativeFixture::new()?;
                let gate = fixture.gate("10000")?;
                let source = fixture.acks.0.join("negative-ack-source.json");
                write_control(&source, &serde_json::to_vec(&fixture.negative_ack()?)?)?;
                match violation {
                    "symlink" => symlink(&source, fixture.ack_path())?,
                    "hardlink" => fs::hard_link(&source, fixture.ack_path())?,
                    _ => {
                        fs::rename(&source, fixture.ack_path())?;
                        fs::set_permissions(fixture.ack_path(), fs::Permissions::from_mode(0o644))?;
                    }
                }
                fixture.assert_zero_creation(&gate).await?;
            }
            Ok(())
        }

        #[tokio::test]
        async fn observer_rejects_receipt_and_ack_directory_swaps_before_engine() -> Result<()> {
            for swap_receipts in [false, true] {
                let fixture = NegativeFixture::new()?;
                let gate = fixture.gate("10000")?;
                let original = if swap_receipts {
                    &fixture.receipts.0
                } else {
                    &fixture.acks.0
                };
                let displaced = fixture
                    .acks
                    .0
                    .with_file_name(format!("drasi-observer-displaced-{}", Uuid::new_v4()));
                fs::rename(original, &displaced)?;
                let displaced = PrivateDirectory(displaced);
                fs::create_dir(original)?;
                fs::set_permissions(original, fs::Permissions::from_mode(0o700))?;
                fixture.assert_zero_creation(&gate).await?;
                drop(displaced);
            }
            Ok(())
        }

        #[tokio::test]
        async fn observer_rejects_changed_prelaunch_manifest_before_engine() -> Result<()> {
            let fixture = NegativeFixture::new()?;
            let gate = fixture.gate("10000")?;
            let path = fixture.acks.0.join("expectations.json");
            OpenOptions::new()
                .append(true)
                .open(path)?
                .write_all(b" ")?;
            fixture.assert_zero_creation(&gate).await
        }

        #[tokio::test]
        async fn observer_binary_replacement_cannot_reach_the_engine_callback() -> Result<()> {
            let mut fixture = NegativeFixture::new()?;
            let path = fixture.acks.0.join("pinned-negative-binary");
            let original = b"immutable compile-handoff fixture bytes";
            let expected = sha256(original);
            write_control(&path, original)?;
            fixture.owner.executable = path.to_str().context("binary path")?.to_owned();
            let mut manifest: Value =
                serde_json::from_slice(&fs::read(fixture.acks.0.join("expectations.json"))?)?;
            manifest["executables"][0]["executable"] = json!(fixture.owner.executable);
            manifest["executables"][0]["executable_sha256"] = json!(expected);
            fixture.replace_manifest(&manifest)?;
            let gate = fixture.gate("10000")?;
            write_control(
                &fixture.ack_path(),
                &serde_json::to_vec(&fixture.negative_ack()?)?,
            )?;
            fs::remove_file(&path)?;
            write_control(&path, b"replacement binary bytes")?;
            fixture.assert_zero_creation(&gate).await
        }

        #[tokio::test]
        async fn observer_rejects_intent_mutation_while_waiting_without_creation() -> Result<()> {
            let fixture = NegativeFixture::new()?;
            let gate = fixture.gate("10000")?;
            let calls = Cell::new(0);
            let creation = fixture.owner.before_creation(1, Some(&gate), || async {
                calls.set(calls.get() + 1);
                Ok(())
            });
            let mutation = async {
                let path = fixture
                    .receipts
                    .0
                    .join(format!("{}-a1.intent.json", fixture.owner.fixture_id));
                OpenOptions::new()
                    .append(true)
                    .open(path)?
                    .write_all(b" ")?;
                Result::<()>::Ok(())
            };
            let (result, mutation) = tokio::join!(biased; creation, mutation);
            mutation?;
            assert!(result.is_err());
            assert_eq!(calls.get(), 0);
            Ok(())
        }

        #[test]
        fn observer_rejects_prelaunch_hash_argv_and_manifest_identity_mismatch() -> Result<()> {
            for field in [
                "executable_sha256",
                "actor_argv",
                "receipt_dir_identity",
                "owner_token",
            ] {
                let mut fixture = NegativeFixture::new()?;
                let mut manifest: Value =
                    serde_json::from_slice(&fs::read(fixture.acks.0.join("expectations.json"))?)?;
                match field {
                    "executable_sha256" => {
                        manifest["executables"][0][field] = json!("0".repeat(64));
                    }
                    "actor_argv" => manifest["executables"][0][field] = json!(["wrong-argv"]),
                    "receipt_dir_identity" => manifest[field] = json!({}),
                    _ => manifest[field] = json!(Uuid::new_v4().to_string()),
                }
                fixture.replace_manifest(&manifest)?;
                assert!(fixture.gate("10000").is_err());
                assert_eq!(fs::read_dir(&fixture.receipts.0)?.count(), 0);
            }
            Ok(())
        }

        #[test]
        fn observer_rejects_ambiguous_or_truncated_command_representations() -> Result<()> {
            for arguments in [
                json!([]),
                json!([""]),
                json!(["/binary", "two arguments"]),
                json!(["/binary", "'quoted'"]),
                json!(["/binary", "\"quoted\""]),
                json!(["/binary", "back\\slash"]),
                json!(["/binary", "line\nbreak"]),
                json!(["x".repeat(4096)]),
            ] {
                assert!(expected_command(&arguments).is_err());
            }
            assert_eq!(
                expected_command(&json!(["/canonical/binary", "--exact", "module::test"]))?,
                "/canonical/binary --exact module::test"
            );
            Ok(())
        }

        #[test]
        fn observer_rejects_same_nested_symlink_and_nonprivate_directories() -> Result<()> {
            let fixture = NegativeFixture::new()?;
            let nested = fixture.receipts.0.join("nested");
            fs::create_dir(&nested)?;
            fs::set_permissions(&nested, fs::Permissions::from_mode(0o700))?;
            let link = fixture.acks.0.join("symlink");
            symlink(&fixture.receipts.0, &link)?;
            for path in [&fixture.receipts.0, &nested, &link] {
                assert!(ObserverAdmission::from_settings(
                    Some(&fixture.owner),
                    Some(path.clone().into_os_string()),
                    Some("10000".into()),
                    Some(fixture.digest.clone().into()),
                )
                .is_err());
            }
            fs::set_permissions(&fixture.acks.0, fs::Permissions::from_mode(0o755))?;
            assert!(fixture.gate("10000").is_err());
            Ok(())
        }

        #[tokio::test]
        async fn observer_missing_ack_times_out_without_engine_creation() -> Result<()> {
            let fixture = NegativeFixture::new()?;
            let gate = fixture.gate("1")?;
            fixture.assert_zero_creation(&gate).await
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    use testcontainers::{GenericImage, Image, ImageExt};

    struct ReceiptDirectory(PathBuf);

    impl ReceiptDirectory {
        fn new() -> Result<Self> {
            let path = std::env::temp_dir()
                .canonicalize()?
                .join(format!("drasi-fixture-receipts-{}", Uuid::new_v4()));
            fs::create_dir(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
            Ok(Self(path))
        }

        fn attribution(&self, name: &'static str, image: String) -> Result<Attribution> {
            Attribution::from_settings(
                Fixture::new(name, image, "fixture/test.rs", 1),
                (OWNER_ENV, DIRECTORY_ENV),
                Some(Uuid::new_v4().to_string().into()),
                Some(self.0.clone().into_os_string()),
            )?
            .context("enabled fixture attribution")
        }
    }

    impl Drop for ReceiptDirectory {
        fn drop(&mut self) {
            if self.0.exists() {
                fs::remove_dir_all(&self.0).expect("remove this test's receipt directory");
            }
        }
    }

    #[test]
    fn neutral_settings_require_both_values_without_redis_aliases() -> Result<()> {
        assert_eq!(OWNER_ENV, "DRASI_TEST_OWNER_TOKEN");
        assert_eq!(DIRECTORY_ENV, "DRASI_TEST_RECEIPT_DIR");
        let fixture = || Fixture::new("wiremock", "wiremock/wiremock:3.3.1", "fixture/test.rs", 1);
        assert!(
            Attribution::from_settings(fixture(), (OWNER_ENV, DIRECTORY_ENV), None, None)?
                .is_none()
        );
        for (owner, directory) in [
            (Some(Uuid::new_v4().to_string().into()), None),
            (None, Some("/missing".into())),
        ] {
            let error =
                Attribution::from_settings(fixture(), (OWNER_ENV, DIRECTORY_ENV), owner, directory)
                    .err()
                    .context("partial settings must fail")?;
            let message = error.to_string();
            assert!(message.contains(OWNER_ENV) && message.contains(DIRECTORY_ENV));
            assert!(!message.contains("DRASI_REDIS_TEST_"));
        }
        Ok(())
    }

    #[test]
    fn selected_image_request_and_receipts_share_exact_attribution_without_a_daemon() -> Result<()>
    {
        let directory = ReceiptDirectory::new()?;
        let image = GenericImage::new("test/selected-fixture", "selected-tag");
        let selected = format!("{}:{}", image.name(), image.tag());
        let owner = directory.attribution("selected", selected.clone())?;
        let request = image
            .with_container_name(owner.container_name(1))
            .with_labels(owner.labels(1));
        owner.record_intent(1)?;
        assert_eq!(request.container_name(), &Some(owner.container_name(1)));
        for (key, value) in owner.labels(1) {
            assert_eq!(request.labels().get(key), Some(&value));
        }
        let intent_path = directory
            .0
            .join(format!("{}-a1.intent.json", owner.fixture_id));
        let intent: Value = serde_json::from_slice(&fs::read(&intent_path)?)?;
        assert_eq!(intent["image"], selected);
        assert_eq!(intent["fixture_source"], "fixture/test.rs");
        assert_eq!(intent["phase"], "creation_intent");
        assert!(intent.get("container_id").is_none());
        assert_eq!(intent["actor_pid"], std::process::id());
        assert_eq!(
            intent["executable"],
            json!(std::env::current_exe()?.canonicalize()?)
        );
        assert_eq!(intent["test_name"], json!(std::thread::current().name()));
        assert_eq!(intent["labels"], json!(owner.labels(1)));
        owner.record_started(1, &"a".repeat(64))?;
        let started_path = directory
            .0
            .join(format!("{}-a1.started.json", owner.fixture_id));
        let mut started: Value = serde_json::from_slice(&fs::read(&started_path)?)?;
        assert_eq!(started["container_id"], "a".repeat(64));
        assert_eq!(started["phase"], "started");
        started
            .as_object_mut()
            .context("started receipt")?
            .remove("container_id");
        started["phase"] = json!("creation_intent");
        assert_eq!(started, intent);
        for path in [intent_path, started_path] {
            let metadata = fs::metadata(path)?;
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
            assert_eq!(metadata.uid(), fs::metadata(&directory.0)?.uid());
        }
        Ok(())
    }

    #[test]
    fn service_receipts_are_single_attempt_and_immutable() -> Result<()> {
        let directory = ReceiptDirectory::new()?;
        let owner = directory.attribution("loki", "grafana/loki:3.4.3".into())?;
        assert!(owner.record_intent(0).is_err());
        assert!(owner.record_intent(2).is_err());
        owner.record_intent(1)?;
        let intent_path = directory
            .0
            .join(format!("{}-a1.intent.json", owner.fixture_id));
        let original = fs::read(&intent_path)?;
        assert!(owner.record_intent(1).is_err());
        for id in ["short".to_owned(), "A".repeat(64), "g".repeat(64)] {
            assert!(owner.record_started(1, &id).is_err());
        }
        assert!(owner.record_started(2, &"a".repeat(64)).is_err());
        assert_eq!(fs::read_dir(&directory.0)?.count(), 1);
        owner.record_started(1, &"a".repeat(64))?;
        let started_path = directory
            .0
            .join(format!("{}-a1.started.json", owner.fixture_id));
        let started = fs::read(&started_path)?;
        assert!(owner.record_started(1, &"b".repeat(64)).is_err());
        assert_eq!(fs::read(&started_path)?, started);
        assert_eq!(fs::read(&intent_path)?, original);
        let other = directory.attribution("loki", "grafana/loki:3.4.3".into())?;
        assert_ne!(owner.container_name(1), other.container_name(1));
        assert_ne!(
            owner.labels(1)["drasi.test.fixture"],
            other.labels(1)["drasi.test.fixture"]
        );
        Ok(())
    }

    #[test]
    fn invalid_neutral_settings_and_metadata_fail_before_receipt_creation() -> Result<()> {
        let directory = ReceiptDirectory::new()?;
        for token in ["", "not-a-uuid", "00000000-0000-0000-0000-000000000000"] {
            assert!(Attribution::from_settings(
                Fixture::new("wiremock", "wiremock/wiremock:3.3.1", "fixture/test.rs", 1),
                (OWNER_ENV, DIRECTORY_ENV),
                Some(token.into()),
                Some(directory.0.clone().into_os_string()),
            )
            .is_err());
        }
        for fixture in [
            Fixture::new("invalid/name", "fixture:tag", "fixture/test.rs", 1),
            Fixture::new("fixture", "", "fixture/test.rs", 1),
            Fixture::new("fixture", "fixture:\ntag", "fixture/test.rs", 1),
            Fixture::new("fixture", "fixture:tag", "", 1),
            Fixture::new("fixture", "fixture:tag", "fixture/test.rs", 0),
        ] {
            assert!(Attribution::from_settings(
                fixture,
                (OWNER_ENV, DIRECTORY_ENV),
                Some(Uuid::new_v4().to_string().into()),
                Some(directory.0.clone().into_os_string()),
            )
            .is_err());
        }
        assert_eq!(fs::read_dir(&directory.0)?.count(), 0);
        Ok(())
    }

    #[test]
    fn service_receipts_reject_symlinks_and_directory_replacement() -> Result<()> {
        let directory = ReceiptDirectory::new()?;
        let target = ReceiptDirectory::new()?;
        let link = directory.0.join("link");
        symlink(&target.0, &link)?;
        assert!(directory_identity(&link).is_err());
        let owner = directory.attribution("wiremock", "wiremock/wiremock:3.3.1".into())?;
        fs::remove_file(link)?;
        let displaced = target.0.join("displaced");
        fs::rename(&directory.0, &displaced)?;
        fs::create_dir(&directory.0)?;
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o700))?;
        assert!(owner.record_intent(1).is_err());
        assert_eq!(fs::read_dir(&directory.0)?.count(), 0);
        Ok(())
    }
}
