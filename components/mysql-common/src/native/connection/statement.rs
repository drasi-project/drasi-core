// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use mysql_common::{packets::ComStmtExecuteRequestBuilder, Value};

impl Connection {
    /// Execute through the same mysql_common prepared-statement encoder used by
    /// the legacy driver. Every result set is consumed before success.
    pub async fn execute_parameters(&mut self, sql: &str, parameters: &[Value]) -> Result<()> {
        timeout(
            self.packets.timeout,
            self.execute_parameters_inner(sql, parameters),
        )
        .await
        .context("MySQL statement timed out; outcome is uncertain")?
    }

    async fn statement_columns(
        &mut self,
        count: usize,
        retained: &mut usize,
        multiple: bool,
    ) -> Result<()> {
        anyhow::ensure!(
            count <= 4096,
            "MySQL statement metadata exceeds column limit"
        );
        for _ in 0..count {
            let packet = self.packets.read().await?;
            check_error(&packet)?;
            account(retained, packet.len(), self.packets.limit)?;
            let mut input = ParseBuf(&packet);
            let _: Column = input.parse(())?;
            anyhow::ensure!(
                input.0.is_empty(),
                "trailing MySQL statement column metadata"
            );
        }
        if count > 0 {
            let end = self.packets.read().await?;
            check_error(&end)?;
            if multiple {
                anyhow::ensure!(
                    end.len() == 5 && end[0] == 0xfe,
                    "invalid MySQL result metadata EOF"
                );
            } else {
                validate_eof(&end)?;
            }
        }
        Ok(())
    }

    async fn execute_parameters_inner(&mut self, sql: &str, parameters: &[Value]) -> Result<()> {
        anyhow::ensure!(
            !sql.trim().is_empty() && sql.len() < self.packets.limit && parameters.len() <= 4096,
            "MySQL command or parameter count exceeds limits"
        );
        let (mut probe, long_data) = ComStmtExecuteRequestBuilder::new(0).build(parameters);
        anyhow::ensure!(
            !long_data,
            "MySQL command exceeds supported inline parameter size"
        );
        let mut command = Vec::new();
        probe.serialize(&mut command);
        anyhow::ensure!(
            command.len() <= self.packets.limit,
            "MySQL parameters exceed protocol byte limit"
        );
        command.clear();
        command.push(0x16);
        command.extend(sql.as_bytes());
        self.command(&command).await?;
        let packet = self.packets.read().await?;
        if let Err(error) = check_error(&packet) {
            if error.downcast_ref::<MySqlServerError>().is_some() {
                self.command_incomplete = false;
            }
            return Err(error);
        }
        anyhow::ensure!(
            packet.len() == 12 && packet[0] == 0 && packet[9] == 0,
            "invalid MySQL statement preparation"
        );
        let id = u32::from_le_bytes(packet[1..5].try_into()?);
        let columns = usize::from(u16::from_le_bytes(packet[5..7].try_into()?));
        let count = usize::from(u16::from_le_bytes(packet[7..9].try_into()?));
        anyhow::ensure!(
            count == parameters.len(),
            "MySQL positional parameter count does not match SQL"
        );
        let mut retained = packet.len();
        self.statement_columns(count, &mut retained, false).await?;
        self.statement_columns(columns, &mut retained, false)
            .await?;
        self.command_incomplete = false;
        (probe, _) = ComStmtExecuteRequestBuilder::new(id).build(parameters);
        command.clear();
        probe.serialize(&mut command);
        self.command(&command).await?;
        self.consume_statement_results().await?;
        command.clear();
        command.push(0x19);
        command.extend(id.to_le_bytes());
        self.command(&command).await?;
        self.command_incomplete = false;
        Ok(())
    }

    async fn consume_statement_results(&mut self) -> Result<()> {
        let mut consumed = 0;
        loop {
            let first = self.packets.read().await?;
            if let Err(error) = check_error(&first) {
                if error.downcast_ref::<MySqlServerError>().is_some() {
                    self.command_incomplete = false;
                }
                return Err(error);
            }
            account(&mut consumed, first.len(), self.packets.limit)?;
            let status = if first.first() == Some(&0) {
                ok_status(&first)?
            } else {
                let mut input = first.as_ref();
                let count = length(&mut input)?.context("MySQL LOCAL INFILE is unsupported")?;
                anyhow::ensure!(
                    input.is_empty() && count > 0,
                    "invalid MySQL result column count"
                );
                self.statement_columns(count, &mut consumed, true).await?;
                loop {
                    let packet = self.packets.read().await?;
                    check_error(&packet)?;
                    account(&mut consumed, packet.len(), self.packets.limit)?;
                    if packet.first() == Some(&0xfe) && packet.len() < 9 {
                        anyhow::ensure!(packet.len() == 5, "invalid MySQL statement EOF");
                        break u16::from_le_bytes([packet[3], packet[4]]);
                    }
                    anyhow::ensure!(
                        packet.first() == Some(&0),
                        "invalid MySQL binary result row"
                    );
                }
            };
            if status & 8 == 0 {
                self.command_incomplete = false;
                anyhow::ensure!(status & 1 == 0, "MySQL procedure left an open transaction");
                return Ok(());
            }
        }
    }
}

fn ok_status(packet: &[u8]) -> Result<u16> {
    anyhow::ensure!(packet.first() == Some(&0), "expected MySQL statement OK");
    let mut data = &packet[1..];
    length(&mut data)?.context("missing MySQL affected rows")?;
    length(&mut data)?.context("missing MySQL last insert ID")?;
    anyhow::ensure!(data.len() >= 4, "truncated MySQL statement OK");
    Ok(u16::from_le_bytes([data[0], data[1]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use testcontainers::{runners::AsyncRunner, ImageExt};
    use testcontainers_modules::mysql::Mysql;

    #[tokio::test]
    #[ignore = "requires isolated Podman MySQL"]
    async fn prepared_calls_bind_values_and_drain_multiple_result_sets() -> Result<()> {
        let database = Mysql::default()
            .with_env_var("MYSQL_DATABASE", "test")
            .with_env_var("MYSQL_ROOT_PASSWORD", "root")
            .start()
            .await?;
        let config = crate::native::tests::config(
            database.get_host_port_ipv4(3306).await?,
            crate::native::MySqlOutput::Changes,
        );
        let mut connection =
            Connection::connect(&config.connection, 1 << 20, Duration::from_secs(5)).await?;
        let result = async {
            connection.execute("CREATE TABLE effect(id BIGINT UNSIGNED PRIMARY KEY, value TEXT, extra JSON, empty_value TEXT)").await?;
            connection.execute("CREATE PROCEDURE write_effect(IN i BIGINT UNSIGNED,IN v TEXT,IN j JSON,IN n TEXT) BEGIN INSERT INTO effect VALUES(i,v,j,n); SELECT i; SELECT v; END").await?;
            connection.execute_parameters("CALL write_effect(?,?,?,?)", &[
                Value::UInt(u64::MAX), Value::Bytes("quote ' and unicode λ".as_bytes().to_vec()),
                Value::Bytes(br#"{"count":17}"#.to_vec()), Value::NULL,
            ]).await?;
            let rows = connection.query("SELECT id,value,CAST(extra AS CHAR),empty_value FROM effect").await?;
            assert_eq!(rows.values.len(), 1);
            assert_eq!(rows.values[0][0].as_deref(), Some(u64::MAX.to_string().as_bytes()));
            assert_eq!(rows.values[0][1].as_deref(), Some("quote ' and unicode λ".as_bytes()));
            assert_eq!(rows.values[0][3], None);
            connection.execute_parameters("UPDATE effect SET value=? WHERE id=?", &[
                Value::Bytes(b"after".to_vec()), Value::UInt(u64::MAX),
            ]).await?;
            assert_eq!(connection.query("SELECT value FROM effect").await?.values[0][0].as_deref(), Some(b"after".as_slice()));
            connection.execute_parameters("DELETE FROM effect WHERE id=?", &[Value::UInt(u64::MAX)]).await?;
            assert!(connection.query("SELECT id FROM effect").await?.values.is_empty());
            Result::<()>::Ok(())
        }.await;
        let cleanup = connection.close().await;
        match (result, cleanup) {
            (Err(primary), Err(cleanup)) => {
                Err(drasi_computation_contracts::OperationFailures::new(
                    "MySQL prepared statement proof and cleanup failed",
                    vec![primary, cleanup],
                )
                .into())
            }
            (Err(error), _) | (_, Err(error)) => Err(error),
            _ => Ok(()),
        }
    }
}
