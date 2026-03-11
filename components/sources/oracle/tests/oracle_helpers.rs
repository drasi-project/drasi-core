// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::{anyhow, Result};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use testcontainers::core::IntoContainerPort;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

#[derive(Debug, Clone)]
pub struct OracleConfig {
    pub host: String,
    pub port: u16,
    pub service: String,
    pub user: String,
    pub password: String,
}

#[derive(Clone)]
pub struct OracleGuard {
    inner: Arc<OracleGuardInner>,
}

struct OracleGuardInner {
    container: std::sync::Mutex<Option<ContainerAsync<GenericImage>>>,
    container_id: String,
    config: OracleConfig,
}

impl OracleGuard {
    pub async fn new() -> Result<Self> {
        let (container, config, container_id) = setup_oracle_raw().await?;
        Ok(Self {
            inner: Arc::new(OracleGuardInner {
                container: std::sync::Mutex::new(Some(container)),
                container_id,
                config,
            }),
        })
    }

    pub fn config(&self) -> &OracleConfig {
        &self.inner.config
    }

    pub fn container_id(&self) -> &str {
        &self.inner.container_id
    }

    pub async fn cleanup(self) {
        let container_to_stop = {
            if let Ok(mut container_guard) = self.inner.container.lock() {
                container_guard.take()
            } else {
                None
            }
        };

        if let Some(container) = container_to_stop {
            let _ = container.stop().await;
        }
    }
}

pub async fn setup_oracle() -> Result<OracleGuard> {
    OracleGuard::new().await
}

pub async fn prepare_oracle_database(oracle: &OracleGuard) -> Result<()> {
    let archivelog_status = exec_sqlplus_as_sysdba(
        oracle.container_id(),
        "SET HEADING OFF FEEDBACK OFF VERIFY OFF PAGESIZE 0\nSELECT LOG_MODE FROM V$DATABASE;\nEXIT;\n",
    )?;

    if !archivelog_status
        .lines()
        .any(|line| line.trim() == "ARCHIVELOG")
    {
        exec_sqlplus_as_sysdba(
            oracle.container_id(),
            "SHUTDOWN IMMEDIATE;\nSTARTUP MOUNT;\nALTER DATABASE ARCHIVELOG;\nALTER DATABASE OPEN;\nEXIT;\n",
        )?;
    }

    let database_sql = r#"
WHENEVER SQLERROR EXIT FAILURE;
BEGIN
  EXECUTE IMMEDIATE 'ALTER DATABASE ADD SUPPLEMENTAL LOG DATA';
EXCEPTION
  WHEN OTHERS THEN
    IF SQLCODE NOT IN (-32588, -32017) THEN RAISE; END IF;
END;
/
EXIT;
"#;
    exec_sqlplus_as_sysdba(oracle.container_id(), database_sql)?;

    let bootstrap_sql = r#"
WHENEVER SQLERROR EXIT FAILURE;
BEGIN
  EXECUTE IMMEDIATE 'DROP TABLE SYSTEM.DRASI_PRODUCTS PURGE';
EXCEPTION
  WHEN OTHERS THEN NULL;
END;
/
CREATE TABLE SYSTEM.DRASI_PRODUCTS (
  ID NUMBER PRIMARY KEY,
  NAME VARCHAR2(100) NOT NULL,
  PRICE NUMBER(10,2) NOT NULL
);
ALTER TABLE SYSTEM.DRASI_PRODUCTS ADD SUPPLEMENTAL LOG DATA (ALL) COLUMNS;
EXIT;
"#;
    exec_sqlplus(
        oracle.container_id(),
        &oracle.config().password,
        bootstrap_sql,
    )?;
    Ok(())
}

pub fn insert_product(oracle: &OracleGuard, id: i64, name: &str, price: f64) -> Result<()> {
    exec_sqlplus(
        oracle.container_id(),
        &oracle.config().password,
        &format!(
            "WHENEVER SQLERROR EXIT FAILURE;\nINSERT INTO SYSTEM.DRASI_PRODUCTS (ID, NAME, PRICE) VALUES ({id}, '{name}', {price});\nCOMMIT;\nEXIT;\n"
        ),
    )?;
    Ok(())
}

pub fn update_product(oracle: &OracleGuard, id: i64, name: &str, price: f64) -> Result<()> {
    exec_sqlplus(
        oracle.container_id(),
        &oracle.config().password,
        &format!(
            "WHENEVER SQLERROR EXIT FAILURE;\nUPDATE SYSTEM.DRASI_PRODUCTS SET NAME = '{name}', PRICE = {price} WHERE ID = {id};\nCOMMIT;\nEXIT;\n"
        ),
    )?;
    Ok(())
}

pub fn delete_product(oracle: &OracleGuard, id: i64) -> Result<()> {
    exec_sqlplus(
        oracle.container_id(),
        &oracle.config().password,
        &format!(
            "WHENEVER SQLERROR EXIT FAILURE;\nDELETE FROM SYSTEM.DRASI_PRODUCTS WHERE ID = {id};\nCOMMIT;\nEXIT;\n"
        ),
    )?;
    Ok(())
}

async fn setup_oracle_raw() -> Result<(ContainerAsync<GenericImage>, OracleConfig, String)> {
    let password = "drasi123";
    let image = GenericImage::new("gvenzl/oracle-free", "slim-faststart")
        .with_exposed_port(1521.tcp())
        .with_env_var("ORACLE_PASSWORD", password)
        .with_env_var("APP_USER", "drasi")
        .with_env_var("APP_USER_PASSWORD", password);

    let container = image.start().await?;
    let port = container.get_host_port_ipv4(1521.tcp()).await?;
    let host = container.get_host().await?.to_string();
    let container_id = container.id().to_string();

    let config = OracleConfig {
        host,
        port,
        service: "FREEPDB1".to_string(),
        user: "system".to_string(),
        password: password.to_string(),
    };

    wait_for_oracle_ready(&container_id, password).await?;
    Ok((container, config, container_id))
}

async fn wait_for_oracle_ready(container_id: &str, password: &str) -> Result<()> {
    let retries = 60;
    for attempt in 1..=retries {
        if let Ok(output) = exec_sqlplus(
            container_id,
            password,
            "SET HEADING OFF FEEDBACK OFF VERIFY OFF PAGESIZE 0\nSELECT 1 FROM DUAL;\nEXIT;\n",
        ) {
            if output.contains('1') {
                return Ok(());
            }
        }

        if attempt < retries {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    Err(anyhow!(
        "Oracle container did not become ready within 300 seconds"
    ))
}

fn exec_sqlplus(container_id: &str, password: &str, sql: &str) -> Result<String> {
    exec_in_container(
        container_id,
        &format!("sqlplus -s system/{password}@//localhost:1521/FREEPDB1"),
        sql,
    )
}

fn exec_sqlplus_as_sysdba(container_id: &str, sql: &str) -> Result<String> {
    exec_in_container(container_id, "sqlplus -s / as sysdba", sql)
}

fn exec_in_container(container_id: &str, command: &str, sql: &str) -> Result<String> {
    let mut child = Command::new("docker")
        .args(["exec", "-i", container_id, "sh", "-lc", command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("failed to open docker exec stdin"))?
        .write_all(sql.as_bytes())?;

    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        return Err(anyhow!(
            "docker exec failed with status {}: {}{}",
            output.status,
            stdout,
            stderr
        ));
    }

    if stdout.contains("ORA-") || stderr.contains("ORA-") || stderr.contains("SP2-") {
        return Err(anyhow!("sqlplus command failed: {stdout}{stderr}"));
    }

    Ok(stdout)
}
