// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

pub(crate) struct Database {
    pub(crate) container: String,
    pub(crate) port: u16,
    pub(crate) volumes: Vec<String>,
    ownership: Option<Value>,
    record: Option<PathBuf>,
}
impl Database {
    pub(crate) fn evidence_directory() -> PathBuf {
        std::env::var_os("DRASI_MYSQL_FIXTURE_RECORDS")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("CARGO_TARGET_DIR")
                    .map(|path| PathBuf::from(path).join("native-mysql-fixtures"))
            })
            .unwrap_or_else(|| std::env::temp_dir().join("drasi-native-mysql-fixtures"))
    }
    pub(crate) async fn new() -> Result<Self> {
        Self::with_gtid(false).await
    }
    pub(crate) async fn with_gtid(gtid: bool) -> Result<Self> {
        Self::start(gtid, false).await
    }
    pub(crate) async fn start(gtid: bool, legacy_sql: bool) -> Result<Self> {
        Self::start_recorded(gtid, legacy_sql, &[], None).await
    }
    pub(crate) async fn start_recorded(
        gtid: bool,
        legacy_sql: bool,
        extra_arguments: &[&str],
        records: Option<&Path>,
    ) -> Result<Self> {
        let output = tokio::process::Command::new("podman")
            .args([
                "run",
                "--detach",
                "--publish",
                "127.0.0.1::3306",
                "--env",
                "MYSQL_ROOT_PASSWORD=native-test",
                "--env",
                "MYSQL_DATABASE=test",
                "--label",
                "drasi.qualification=native-mysql-family",
            ])
            .args(if legacy_sql {
                &["--platform", "linux/amd64", "mysql:5.7.44"][..]
            } else {
                &["mysql:8.4"][..]
            })
            .args([
                "--log-bin=mysql-bin",
                "--binlog-format=ROW",
                "--binlog-row-image=FULL",
                "--server-id=1",
            ])
            .args(if legacy_sql {
                &["--expire-logs-days=0"][..]
            } else {
                &[
                    "--binlog-row-metadata=FULL",
                    "--binlog-expire-logs-seconds=0",
                ][..]
            })
            .args(if gtid {
                &["--gtid-mode=ON", "--enforce-gtid-consistency=ON"][..]
            } else {
                &[]
            })
            .args(extra_arguments)
            .kill_on_drop(true)
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            "Podman MySQL startup: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let container = String::from_utf8(output.stdout)?.trim().to_owned();
        anyhow::ensure!(
            container.len() == 64 && container.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid owned container ID"
        );
        let mut database = Self {
            container,
            port: 0,
            volumes: Vec::new(),
            ownership: None,
            record: None,
        };
        database.record_ownership(records)?;
        let output = tokio::process::Command::new("podman")
            .args(["port", &database.container, "3306/tcp"])
            .output()
            .await?;
        anyhow::ensure!(output.status.success(), "MySQL fixture port unavailable");
        database.port = String::from_utf8(output.stdout)?
            .trim()
            .strip_prefix("127.0.0.1:")
            .context("loopback binding")?
            .parse()?;
        let ready = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let ready = tokio::process::Command::new("podman")
                    .args([
                        "exec",
                        &database.container,
                        "mysqladmin",
                        "ping",
                        "-h",
                        "127.0.0.1",
                        "-uroot",
                        "-pnative-test",
                    ])
                    .output()
                    .await?;
                if ready.status.success() {
                    return Result::<()>::Ok(());
                }
                let state = tokio::process::Command::new("podman")
                    .args([
                        "inspect",
                        "--format",
                        "{{.State.Running}}",
                        &database.container,
                    ])
                    .output()
                    .await?;
                anyhow::ensure!(
                    state.status.success() && state.stdout.starts_with(b"true"),
                    "owned MySQL fixture exited before readiness"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result);
        if let Err(error) = ready {
            let logs = tokio::process::Command::new("podman")
                .args(["logs", "--tail", "40", &database.container])
                .output()
                .await;
            let diagnostic = match logs {
                Ok(logs) => format!(
                    "{} {}",
                    String::from_utf8_lossy(&logs.stdout),
                    String::from_utf8_lossy(&logs.stderr),
                ),
                Err(error) => format!("could not read fixture logs: {error}"),
            };
            return Err(error.context(format!(
                "MySQL fixture {} failed to start; ownership record {:?}; logs: {}",
                database.container, database.record, diagnostic,
            )));
        }
        Ok(database)
    }
    fn record_ownership(&mut self, directory: Option<&Path>) -> Result<()> {
        let inspected = Command::new("podman")
            .args(["inspect", &self.container])
            .output()?;
        anyhow::ensure!(
            inspected.status.success(),
            "cannot inspect owned MySQL fixture {}",
            self.container
        );
        let values: Vec<Value> = serde_json::from_slice(&inspected.stdout)?;
        let value = values.first().context("owned MySQL container inspection")?;
        anyhow::ensure!(
            value["Id"].as_str() == Some(self.container.as_str())
                && value["Config"]["Labels"]["drasi.qualification"] == "native-mysql-family",
            "MySQL fixture ownership changed; refusing cleanup"
        );
        let mounts = value["Mounts"].as_array().context("MySQL volume mounts")?;
        self.volumes.clear();
        for mount in mounts.iter().filter(|mount| mount["Type"] == "volume") {
            let name = mount["Name"].as_str().context("owned MySQL volume name")?;
            anyhow::ensure!(
                name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "unexpected MySQL fixture volume identity"
            );
            let inspected = Command::new("podman")
                .args(["volume", "inspect", "--format", "{{.Anonymous}}", name])
                .output()?;
            anyhow::ensure!(
                inspected.status.success() && inspected.stdout.starts_with(b"true"),
                "MySQL fixture has a non-anonymous volume; refusing automatic volume removal"
            );
            self.volumes.push(name.to_owned());
        }
        anyhow::ensure!(
            !self.volumes.is_empty(),
            "MySQL fixture data volume missing"
        );
        let directory = directory
            .map(Path::to_path_buf)
            .unwrap_or_else(Self::evidence_directory);
        std::fs::create_dir_all(&directory)?;
        self.record = Some(directory.join(format!("{}.json", self.container)));
        self.ownership = Some(json!({
            "container":self.container,"label":"native-mysql-family",
            "image":value["ImageName"],"mounts":mounts,"volumes":self.volumes,"removed":false
        }));
        self.write_record(false)
    }
    fn write_record(&self, removed: bool) -> Result<()> {
        let mut record = self
            .ownership
            .clone()
            .context("MySQL ownership evidence missing")?;
        record["removed"] = json!(removed);
        if removed {
            record["remaining_owned_volume_count"] = json!(0);
            record["remaining_owned_volume_bytes"] = json!(0);
        }
        std::fs::write(
            self.record
                .as_ref()
                .context("MySQL ownership record path")?,
            serde_json::to_vec_pretty(&record)?,
        )?;
        Ok(())
    }
    pub(crate) async fn sql(&self, sql: &str) -> Result<String> {
        let output = tokio::process::Command::new("podman")
            .args([
                "exec",
                &self.container,
                "mysql",
                "-uroot",
                "-pnative-test",
                "--batch",
                "--skip-column-names",
                "test",
                "-e",
                sql,
            ])
            .kill_on_drop(true)
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            "MySQL fixture SQL: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }
    pub(crate) async fn expect_count(&self, statement: &str, expected: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(20), async {
            while self.sql(statement).await? != expected {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Result::<()>::Ok(())
        })
        .await?
    }
    pub(crate) async fn initialize(&self) -> Result<()> {
        self.initialize_columns("id INT PRIMARY KEY,value VARCHAR(80) NOT NULL")
            .await
    }
    pub(crate) async fn initialize_columns(&self, columns: &str) -> Result<()> {
        self.sql(&format!("CREATE TABLE items({columns}); INSERT INTO items(id,value) VALUES(1,'initial'); \
            CREATE TABLE effects(query_id VARCHAR(64),id INT,value TEXT,PRIMARY KEY(query_id,id)); \
            CREATE TABLE audit(query_id VARCHAR(64),operation VARCHAR(16),id INT);\n\
            DELIMITER //\n\
            CREATE PROCEDURE effect(IN q VARCHAR(64),IN op VARCHAR(16),IN i INT,IN v TEXT) BEGIN \
            INSERT INTO audit VALUES(q,op,i); IF op='DELETE' THEN DELETE FROM effects WHERE query_id=q AND id=i; \
            ELSE INSERT INTO effects VALUES(q,i,v) ON DUPLICATE KEY UPDATE value=v; END IF; END //\n\
            CREATE PROCEDURE wait_effect(IN q VARCHAR(64)) BEGIN DO SLEEP(1); INSERT INTO audit VALUES(q,'wait',900); END //\n\
            CREATE PROCEDURE fail_effect(IN q VARCHAR(64)) BEGIN INSERT INTO audit VALUES(q,'failed',901); \
            SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT='expected failure'; END //\n\
            DELIMITER ;\n")).await?;
        Ok(())
    }
    pub(crate) fn source_settings(&self) -> Value {
        json!({
            "connection":{"host":"127.0.0.1","port":self.port,"database":"test","user":"root","password":"native-test",
                "ssl_mode":"disabled","tls_ca_pem":null,"server_id":811,"heartbeat_interval_ms":100},
            "tables":["items"],"start":{"mode":"snapshot","lock_timeout_ms":5000},"output":"changes","replay":"server",
            "transactions":{"max_changes":64,"max_bytes":1048576,"max_duration_ms":10000},
            "max_protocol_bytes":1048576,"io_timeout_ms":10000
        })
    }
    pub(crate) fn sink_settings(&self) -> Value {
        json!({
            "hostname":"127.0.0.1","port":self.port,"database":"test","user":"root","password":"native-test",
            "queries":["left","right"],
            "default_template":{
                "added":{"template":"CALL effect({{param query_id}},{{param operation}},{{param after.id}},{{param after.value}})"},
                "updated":{"template":"CALL effect({{param query_id}},{{param operation}},{{param after.id}},{{param after.value}})"},
                "deleted":{"template":"CALL effect({{param query_id}},{{param operation}},{{param before.id}},NULL)"}
            }
        })
    }
    pub(crate) fn cleanup(&mut self) -> Result<()> {
        if self.container.is_empty() {
            return Ok(());
        }
        if self.ownership.is_none() {
            self.record_ownership(None)?;
        }
        let result = Command::new("podman")
            .args(["rm", "--force", "--volumes", &self.container])
            .output()?;
        anyhow::ensure!(
            result.status.success(),
            "owned MySQL fixture cleanup failed; evidence {:?}: {}",
            self.record,
            String::from_utf8_lossy(&result.stderr)
        );
        self.container.clear();
        for name in &self.volumes {
            let status = Command::new("podman")
                .args(["volume", "exists", name])
                .status()?;
            anyhow::ensure!(
                status.code() == Some(1),
                "owned MySQL volume {name} remains or cannot be verified; evidence {:?}",
                self.record,
            );
        }
        self.write_record(true)?;
        Ok(())
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            log::error!("MySQL fixture cleanup: {error:#}");
        }
    }
}
