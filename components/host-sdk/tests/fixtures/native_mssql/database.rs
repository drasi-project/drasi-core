// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    io::Write,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
struct Credentials {
    user: String,
    password: String,
}
#[derive(Clone)]
struct Endpoint {
    container: Option<String>,
    executable: String,
    host: String,
    port: u16,
    database: String,
    trust_certificate: bool,
}
pub struct Database {
    endpoint: Endpoint,
    admin: Credentials,
    application: Credentials,
    reader: Credentials,
    pub schema: String,
    token: String,
    owns_schema: bool,
}

fn required(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("{name} is required"))?;
    ensure!(
        !value.is_empty() && !value.contains('\0'),
        "{name} must be nonempty without NUL"
    );
    Ok(value)
}
fn credentials(prefix: &str) -> Result<Credentials> {
    let user = required(&format!("{prefix}_USER"))?;
    ensure!(
        user.len() <= 128
            && user
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-.@".contains(&c)),
        "{prefix}_USER must be a simple SQL login/database-user identifier"
    );
    Ok(Credentials {
        user,
        password: required(&format!("{prefix}_PASSWORD"))?,
    })
}
fn identifier(value: &str) -> String {
    format!("[{}]", value.replace(']', "]]"))
}
fn literal(value: &str) -> String {
    format!("N'{}'", value.replace('\'', "''"))
}
fn unique_token() -> Result<String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    Ok(format!(
        "{:x}_{:x}_{:x}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

impl Endpoint {
    fn command(&self, credentials: &Credentials) -> Command {
        let mut command = if let Some(container) = &self.container {
            let mut command = Command::new("podman");
            command.args([
                "exec",
                "-i",
                "--env",
                "SQLCMDPASSWORD",
                container,
                &self.executable,
            ]);
            command
        } else {
            Command::new(&self.executable)
        };
        let server = if self.container.is_some() {
            "localhost".into()
        } else {
            format!("tcp:{},{}", self.host, self.port)
        };
        command.args([
            "-S",
            &server,
            "-d",
            &self.database,
            "-U",
            &credentials.user,
            "-b",
            "-h",
            "-1",
            "-y",
            "0",
            "-w",
            "65535",
            "-s",
            "|",
            "-f",
            "65001",
            "-l",
            "5",
            "-t",
            "40",
        ]);
        if self.trust_certificate {
            command.arg("-C");
        }
        // Never put passwords in argv, persisted fixture files or diagnostic output.
        command
            .env("SQLCMDPASSWORD", &credentials.password)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

/// A real SQL command process, retained if its async waiter is cancelled.
/// The fixture's SQLCMD timeout bounds cleanup; native cancellation assertions
/// use server-visible effect records, not subprocess/socket disappearance.
pub struct SqlJob {
    child: Option<Child>,
    redactions: Vec<String>,
}
impl SqlJob {
    pub fn pending(&self) -> bool {
        self.child.is_some()
    }
    pub async fn finish(&mut self) -> Result<String> {
        loop {
            if self
                .child
                .as_mut()
                .context("SQL job already consumed")?
                .try_wait()?
                .is_some()
            {
                return self.finish_blocking();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    fn finish_blocking(&mut self) -> Result<String> {
        let output = self
            .child
            .take()
            .context("SQL job already consumed")?
            .wait_with_output()?;
        let sanitize = |bytes: &[u8]| {
            self.redactions.iter().fold(
                String::from_utf8_lossy(bytes).into_owned(),
                |text, secret| text.replace(secret, "[REDACTED]"),
            )
        };
        ensure!(
            output.status.success(),
            "isolated SQL command failed: {} {}",
            sanitize(&output.stdout),
            sanitize(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }
}
impl Drop for SqlJob {
    fn drop(&mut self) {
        if self.child.is_some() {
            if let Err(error) = self.finish_blocking() {
                eprintln!("retained SQL fixture command cleanup failed: {error:#}");
            }
        }
    }
}

impl Database {
    pub async fn new() -> Result<Self> {
        let mode = required("DRASI_MSSQL_TEST_MODE")?;
        let token = unique_token()?;
        let mut database = match mode.as_str() {
            "external" => {
                ensure!(
                    required("DRASI_MSSQL_TEST_ALLOW_EXTERNAL")? == "isolated-database",
                    "external SQL Server requires explicit isolated-database consent"
                );
                let name = required("DRASI_MSSQL_TEST_DATABASE")?;
                ensure!(
                    name.starts_with("drasi_native_")
                        && name.len() <= 128
                        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                    "external database must have an explicit drasi_native_ identifier"
                );
                let prefix = required("DRASI_MSSQL_TEST_SCHEMA_PREFIX")?;
                ensure!(prefix.starts_with("drasi_native_") && prefix.len() <= 28 &&
                    prefix.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                    "schema prefix must be a lowercase drasi_native_ identifier of at most 28 bytes");
                let trust = match required("DRASI_MSSQL_TEST_TRUST_CERTIFICATE")?.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => anyhow::bail!("DRASI_MSSQL_TEST_TRUST_CERTIFICATE must be true or false"),
                };
                let database = Self {
                    endpoint: Endpoint {
                        container: None,
                        executable: required("DRASI_MSSQL_TEST_SQLCMD")?,
                        host: required("DRASI_MSSQL_TEST_HOST")?,
                        port: required("DRASI_MSSQL_TEST_PORT")?.parse()?,
                        database: name,
                        trust_certificate: trust,
                    },
                    admin: credentials("DRASI_MSSQL_TEST_ADMIN")?,
                    application: credentials("DRASI_MSSQL_TEST_APP")?,
                    reader: credentials("DRASI_MSSQL_TEST_READER")?,
                    schema: format!("{prefix}_{token}"),
                    token: token.clone(),
                    owns_schema: false,
                };
                ensure!(
                    database.endpoint.port > 0,
                    "SQL Server port must be positive"
                );
                let ownership = required("DRASI_MSSQL_TEST_OWNERSHIP_TOKEN")?;
                ensure!((32..=256).contains(&ownership.len()) &&
                    ownership.bytes().all(|c|c.is_ascii_alphanumeric() || b"_-".contains(&c)),
                    "external ownership token must contain 32..=256 ASCII letters/digits, underscore or hyphen");
                let sql = format!(
                    "SET NOCOUNT ON; SELECT CASE WHEN DB_NAME()={} AND \
                     (SELECT CONVERT(nvarchar(256),value) FROM sys.extended_properties \
                      WHERE class=0 AND name=N'DrasiNativeQualificationOwner')={} \
                     AND (SELECT is_cdc_enabled FROM sys.databases WHERE name=DB_NAME())=1 \
                     AND (SELECT snapshot_isolation_state FROM sys.databases WHERE name=DB_NAME())=1 \
                     THEN 1 ELSE 0 END;",
                    literal(&database.endpoint.database),literal(&ownership));
                ensure!(database.raw_sql(&database.admin,&sql)? == "1",
                    "external database ownership/CDC/SNAPSHOT preflight failed; nothing provisioned");
                ensure!(database.raw_sql(&database.application,
                    "SET NOCOUNT ON; SELECT CASE WHEN HAS_PERMS_BY_NAME(N'cdc.change_tables',N'OBJECT',N'SELECT')=1 AND IS_MEMBER(N'db_owner')=0 AND IS_SRVROLEMEMBER(N'sysadmin')=0 THEN 1 ELSE 0 END")?=="1",
                    "external application needs pre-provisioned CDC catalog SELECT without administrator membership");
                database
            }
            "podman" => {
                let password = required("DRASI_MSSQL_TEST_LOCAL_PASSWORD")?;
                ensure!(
                    !password.chars().any(char::is_control),
                    "local fixture password must not contain control characters"
                );
                let image = std::env::var("DRASI_MSSQL_TEST_IMAGE").unwrap_or_else(|_| {
                    "mcr.microsoft.com/mssql/server:2022-CU22-ubuntu-22.04".into()
                });
                let output = Command::new("podman")
                    .args([
                        "run",
                        "--detach",
                        "--platform",
                        "linux/amd64",
                        "--publish",
                        "127.0.0.1::1433",
                        "--label",
                        "drasi.qualification=native-mssql",
                        "--env",
                        "ACCEPT_EULA=Y",
                        "--env",
                        "MSSQL_SA_PASSWORD",
                        "--env",
                        "MSSQL_PID=Developer",
                        "--env",
                        "MSSQL_AGENT_ENABLED=true",
                        &image,
                    ])
                    .env("MSSQL_SA_PASSWORD", &password)
                    .output()?;
                ensure!(
                    output.status.success(),
                    "owned Podman SQL Server start failed"
                );
                let container = String::from_utf8(output.stdout)?.trim().to_owned();
                ensure!(
                    container.len() == 64 && container.bytes().all(|b| b.is_ascii_hexdigit()),
                    "invalid owned container"
                );
                let mut database = Self {
                    endpoint: Endpoint {
                        container: Some(container.clone()),
                        executable: "/opt/mssql-tools18/bin/sqlcmd".into(),
                        host: "127.0.0.1".into(),
                        port: 0,
                        database: "master".into(),
                        trust_certificate: true,
                    },
                    admin: Credentials {
                        user: "sa".into(),
                        password: password.clone(),
                    },
                    application: Credentials {
                        user: "native_application".into(),
                        password: password.clone(),
                    },
                    reader: Credentials {
                        user: "native_readonly".into(),
                        password,
                    },
                    schema: format!("drasi_native_{token}"),
                    token: token.clone(),
                    owns_schema: false,
                };
                let port = Command::new("podman")
                    .args(["port", &container, "1433/tcp"])
                    .output()?;
                ensure!(port.status.success(), "owned SQL Server port lookup failed");
                database.endpoint.port = String::from_utf8(port.stdout)?
                    .trim()
                    .strip_prefix("127.0.0.1:")
                    .context("loopback port")?
                    .parse()?;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
                loop {
                    let output = Command::new("podman")
                        .args([
                            "inspect",
                            "--format",
                            "{{.State.Status}} {{.State.ExitCode}}",
                            &container,
                        ])
                        .output()?;
                    let status = String::from_utf8(output.stdout)?;
                    ensure!(
                        status.starts_with("running ") && tokio::time::Instant::now() < deadline,
                        "real SQL Server not ready ({status}); no ARM/emulation skip"
                    );
                    if database
                        .raw_sql(&database.admin, "SET NOCOUNT ON; SELECT 1")
                        .is_ok()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                database.raw_sql(&database.admin,"CREATE DATABASE NativeQualification; ALTER DATABASE NativeQualification SET ALLOW_SNAPSHOT_ISOLATION ON;")?;
                for credentials in [&database.application, &database.reader] {
                    database.raw_sql(
                        &database.admin,
                        &format!(
                            "CREATE LOGIN {} WITH PASSWORD={},CHECK_POLICY=OFF",
                            identifier(&credentials.user),
                            literal(&credentials.password)
                        ),
                    )?;
                }
                database.endpoint.database = "NativeQualification".into();
                database.raw_sql(&database.admin, "EXEC sys.sp_cdc_enable_db")?;
                for credentials in [&database.application, &database.reader] {
                    database.raw_sql(
                        &database.admin,
                        &format!(
                            "CREATE USER {} FOR LOGIN {}",
                            identifier(&credentials.user),
                            identifier(&credentials.user)
                        ),
                    )?;
                }
                database.raw_sql(
                    &database.admin,
                    &format!(
                        "GRANT SELECT ON OBJECT::cdc.change_tables TO {}",
                        identifier(&database.application.user)
                    ),
                )?;
                database
            }
            _ => anyhow::bail!("DRASI_MSSQL_TEST_MODE must explicitly select external or podman"),
        };
        ensure!(
            database.schema.len() <= 90,
            "owned schema/capture identifier exceeds SQL Server CDC limit"
        );
        database.raw_sql(
            &database.admin,
            &format!(
                "SET XACT_ABORT ON; BEGIN TRANSACTION; EXEC(N'CREATE SCHEMA {} AUTHORIZATION dbo'); \
             CREATE TABLE {}.__DrasiOwned(token nvarchar(128) NOT NULL); \
             INSERT INTO {}.__DrasiOwned VALUES({}); COMMIT",
                identifier(&database.schema),
                identifier(&database.schema),
                identifier(&database.schema),
                literal(&database.token)
            ),
        )?;
        database.owns_schema = true;
        let setup = include_str!("setup.sql")
            .replace("{{app_user}}", &identifier(&database.application.user))
            .replace("{{reader_user}}", &identifier(&database.reader.user))
            .replace("{{schema_name}}", &database.schema);
        database.sql(&setup)?;
        Ok(database)
    }

    pub fn scoped_sql(&self, sql: &str) -> String {
        sql.replace("{{schema}}", &identifier(&self.schema))
            .replace("{{schema_name}}", &self.schema)
            .replace("{{owned_token}}", &self.token)
    }
    fn spawn_as(&self, credentials: &Credentials, sql: &str) -> Result<SqlJob> {
        let mut child = self
            .endpoint
            .command(credentials)
            .spawn()
            .context("start explicitly selected SQLCMD executable")?;
        child
            .stdin
            .take()
            .context("SQL command stdin")?
            .write_all(sql.as_bytes())?;
        Ok(SqlJob {
            child: Some(child),
            redactions: vec![
                self.admin.password.clone(),
                self.application.password.clone(),
                self.reader.password.clone(),
                self.admin.user.clone(),
                self.application.user.clone(),
                self.reader.user.clone(),
            ],
        })
    }
    fn raw_sql(&self, credentials: &Credentials, sql: &str) -> Result<String> {
        self.spawn_as(credentials, sql)?.finish_blocking()
    }
    pub fn sql(&self, sql: &str) -> Result<String> {
        self.raw_sql(&self.admin, &self.scoped_sql(sql))
    }
    pub fn application_sql(&self, sql: &str) -> Result<String> {
        self.raw_sql(&self.application, &self.scoped_sql(sql))
    }
    pub fn reader_sql(&self, sql: &str) -> Result<String> {
        self.raw_sql(&self.reader, &self.scoped_sql(sql))
    }
    pub fn regrant_items_capture(&self) -> Result<()> {
        self.sql(&format!(
            "GRANT SELECT ON OBJECT::cdc.[{}_Items_CT] TO {};
            GRANT SELECT ON OBJECT::cdc.[fn_cdc_get_all_changes_{}_Items] TO {};",
            self.schema,
            identifier(&self.application.user),
            self.schema,
            identifier(&self.application.user)
        ))?;
        Ok(())
    }
    pub fn spawn_sql(&self, sql: &str) -> Result<SqlJob> {
        self.spawn_as(&self.admin, &self.scoped_sql(sql))
    }
    pub async fn wait_sql(&self, sql: &str, expected: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                if self.sql(sql)? == expected {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("real SQL Server observation deadline")?
    }
    pub fn settings(&self) -> Value {
        json!({"connection":{
            "host":self.endpoint.host,"port":self.endpoint.port,"database":self.endpoint.database,
            "user":self.application.user,"password":self.application.password,
            "tables":[format!("{}.Items",self.schema)],"encryption":"on",
            "trust_server_certificate":self.endpoint.trust_certificate,"poll_interval_ms":25},
            "barrier_table":format!("{}.DrasiBarrier",self.schema),"max_rows":100000,"max_bytes":16777216,
            "operation_timeout_ms":15000,"shutdown_timeout_ms":15000})
    }
    pub fn read_only_settings(&self) -> Value {
        let mut settings = self.settings();
        settings
            .as_object_mut()
            .expect("settings")
            .remove("barrier_table");
        settings["connection"]["user"] = json!(self.reader.user);
        settings["connection"]["password"] = json!(self.reader.password);
        settings["connection"]["tables"] = json!([format!("{}.ReadOnlyItems", self.schema)]);
        settings
    }
    pub fn remove(&mut self) -> Result<()> {
        if let Some(container) = self.endpoint.container.as_ref() {
            // Final fixture disposal only. Component/graph restarts keep the
            // container and its data; named volumes and bind mounts are not pruned.
            let output = Command::new("podman")
                .args(["rm", "--force", "--volumes", container])
                .output()?;
            ensure!(
                output.status.success(),
                "owned SQL Server container cleanup failed"
            );
            self.endpoint.container = None;
            self.owns_schema = false;
            return Ok(());
        }
        if self.owns_schema {
            ensure!(
                self.sql("SET NOCOUNT ON; SELECT token FROM {{schema}}.__DrasiOwned")?
                    == self.token,
                "owned-schema token changed; refusing external cleanup"
            );
            self.sql(include_str!("cleanup.sql"))?;
            self.owns_schema = false;
        }
        Ok(())
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            eprintln!("owned SQL Server fixture cleanup failed: {error:#}");
        }
    }
}
