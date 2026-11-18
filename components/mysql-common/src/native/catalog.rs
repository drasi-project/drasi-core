// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{connection::Connection, MySqlConfig, MySqlRetention};
use anyhow::{Context, Result};
use mysql_common::{binlog::row::BinlogRow, constants::ColumnFlags};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize)]
pub struct Column {
    pub name: String,
    pub sql_type: String,
    pub key: bool,
    pub nullable: bool,
    pub charset: Option<String>,
    pub collation: Option<u16>,
}

pub type Tables = BTreeMap<String, Vec<Column>>;

pub fn require_replication_version(connection: &Connection) -> Result<()> {
    if connection.version < (8, 0, 20) {
        return Err(super::errors::unsupported(
            "native replication requires MySQL 8.0.20 or newer",
        ));
    }
    Ok(())
}

pub fn field(row: &[Option<Vec<u8>>], index: usize) -> Result<&str> {
    Ok(std::str::from_utf8(
        row.get(index)
            .and_then(Option::as_deref)
            .context("missing MySQL catalog value")?,
    )?)
}

/// The selected catalog of one inspection.
#[derive(Debug, Clone, Default)]
pub struct Inspection {
    /// `@@server_uuid`.
    pub server: String,
    /// Selected tables and their columns.
    pub tables: Tables,
    /// Tables of a database-wide selection skipped because the source cannot
    /// capture them (unsupported type, character set, engine or key).
    pub skipped: BTreeSet<String>,
}

/// Which catalog contract an inspection enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Whole transactions or a coordinated snapshot: InnoDB tables only.
    Transactional,
    /// Row changes: any engine.
    Rows,
    /// Positional rows (`@N` keys) without full row metadata.
    Positional,
}

impl Profile {
    /// The profile a source with this configuration uses.
    pub fn for_config(config: &MySqlConfig, positional: bool) -> Self {
        if positional {
            Self::Positional
        } else if config.output == super::MySqlOutput::Transactions
            || matches!(config.start, super::MySqlStartPosition::Snapshot { .. })
        {
            Self::Transactional
        } else {
            Self::Rows
        }
    }
}

pub async fn inspect(
    connection: &mut Connection,
    config: &MySqlConfig,
) -> Result<(String, Tables)> {
    let found = inspect_profile(connection, config, Profile::Transactional).await?;
    Ok((found.server, found.tables))
}

pub async fn inspect_rows(
    connection: &mut Connection,
    config: &MySqlConfig,
) -> Result<(String, Tables)> {
    let profile = if matches!(config.start, super::MySqlStartPosition::Snapshot { .. }) {
        Profile::Transactional
    } else {
        Profile::Rows
    };
    let found = inspect_profile(connection, config, profile).await?;
    Ok((found.server, found.tables))
}

pub async fn inspect_positional(
    connection: &mut Connection,
    config: &MySqlConfig,
) -> Result<(String, Tables)> {
    let found = inspect_profile(connection, config, Profile::Positional).await?;
    Ok((found.server, found.tables))
}

/// Checks the server's replication settings and reads the selected catalog.
pub async fn inspect_profile(
    connection: &mut Connection,
    config: &MySqlConfig,
    profile: Profile,
) -> Result<Inspection> {
    let positional = profile == Profile::Positional;
    if !positional {
        require_replication_version(connection)?;
    }
    let rows = connection
        .query(if positional {
            "SELECT @@server_uuid, @@global.binlog_format, @@global.binlog_row_image, \
             @@global.binlog_checksum"
        } else {
            "SELECT @@server_uuid, @@global.binlog_format, @@global.binlog_row_image, \
             @@global.binlog_row_metadata, @@global.binlog_checksum"
        })
        .await?;
    let [settings] = rows.values.as_slice() else {
        anyhow::bail!("invalid MySQL server settings result");
    };
    if !(field(settings, 1)? == "ROW"
        && field(settings, 2)? == "FULL"
        && (positional || field(settings, 3)? == "FULL")
        && field(settings, if positional { 3 } else { 4 })? == "CRC32")
    {
        return Err(super::errors::unsupported(
            "native MySQL requires binlog_format=ROW, binlog_row_image=FULL, \
             binlog_checksum=CRC32 and (except positional mode) binlog_row_metadata=FULL",
        ));
    }
    if config.replay == Some(MySqlRetention::UntilProcessed) {
        let retention = connection
            .query(
                "SHOW GLOBAL VARIABLES WHERE Variable_name IN \
            ('binlog_expire_logs_seconds','expire_logs_days','binlog_expire_logs_auto_purge')",
            )
            .await?;
        if positional && connection.version < (8, 0, 0) {
            anyhow::ensure!(
                retention.values.iter().any(|row| {
                    field(row, 0).is_ok_and(|name| name == "expire_logs_days")
                        && field(row, 1)
                            .is_ok_and(|value| value.parse::<f64>().is_ok_and(|days| days == 0.0))
                }),
                "positional retention until processing requires MySQL 5.7 expire_logs_days=0"
            );
        } else {
            validate_retention(&retention.values)
                .map_err(|error| super::errors::unsupported(format!("{error:#}")))?;
        }
    }
    let server = field(settings, 0)?.to_string();
    anyhow::ensure!(!server.is_empty(), "missing MySQL server UUID");
    let (tables, skipped) = read_tables(
        connection,
        config,
        profile == Profile::Transactional,
        false,
        positional,
    )
    .await?;
    Ok(Inspection {
        server,
        tables,
        skipped,
    })
}

/// Reads the current catalog of `config.tables` without checking server
/// settings. Missing tables are absent from the result; an unsupported table
/// is an error.
pub async fn inspect_tables(
    connection: &mut Connection,
    config: &MySqlConfig,
    profile: Profile,
) -> Result<Inspection> {
    let server = super::session::server_id(connection).await?;
    let (tables, skipped) = read_tables_with(
        connection,
        config,
        profile == Profile::Transactional,
        false,
        profile == Profile::Positional,
        false,
    )
    .await?;
    Ok(Inspection {
        server,
        tables,
        skipped,
    })
}

pub async fn inspect_standalone(
    connection: &mut Connection,
    config: &MySqlConfig,
) -> Result<Tables> {
    Ok(read_tables(connection, config, false, true, false).await?.0)
}

/// A string literal that needs no escaping in any SQL mode: a hex literal
/// with a `utf8mb4` introducer.
pub fn sql_text(value: &str) -> String {
    if value.is_empty() {
        return "''".into();
    }
    let mut literal = String::with_capacity(value.len() * 2 + 12);
    literal.push_str("_utf8mb4 0x");
    for byte in value.as_bytes() {
        literal.push_str(&format!("{byte:02x}"));
    }
    literal
}

fn text_list<'a>(values: impl IntoIterator<Item = &'a String>) -> String {
    values
        .into_iter()
        .map(|value| sql_text(value))
        .collect::<Vec<_>>()
        .join(",")
}

async fn read_tables(
    connection: &mut Connection,
    config: &MySqlConfig,
    transactional: bool,
    standalone: bool,
    positional: bool,
) -> Result<(Tables, BTreeSet<String>)> {
    read_tables_with(
        connection,
        config,
        transactional,
        standalone,
        positional,
        true,
    )
    .await
}

async fn read_tables_with(
    connection: &mut Connection,
    config: &MySqlConfig,
    transactional: bool,
    standalone: bool,
    positional: bool,
    require_existing: bool,
) -> Result<(Tables, BTreeSet<String>)> {
    let selection = if config.tables.is_empty() {
        String::new()
    } else {
        format!(" AND c.TABLE_NAME IN ({})", text_list(&config.tables))
    };
    let table_kind = if standalone {
        ""
    } else {
        " AND t.TABLE_TYPE='BASE TABLE'"
    };
    let database = sql_text(&config.connection.database);
    let rows = connection
        .query(&format!(
            "SELECT c.TABLE_NAME, c.COLUMN_NAME, c.COLUMN_TYPE, c.COLUMN_KEY, \
        c.IS_NULLABLE, c.CHARACTER_SET_NAME, t.ENGINE, coll.ID \
        FROM information_schema.COLUMNS c JOIN information_schema.TABLES t \
        ON c.TABLE_SCHEMA=t.TABLE_SCHEMA AND c.TABLE_NAME=t.TABLE_NAME \
        LEFT JOIN information_schema.COLLATIONS coll ON c.COLLATION_NAME=coll.COLLATION_NAME \
        WHERE c.TABLE_SCHEMA={database}{table_kind}{selection} \
        ORDER BY c.TABLE_NAME, c.ORDINAL_POSITION"
        ))
        .await?;
    // A database-wide selection skips what it cannot capture; an explicit
    // selection fails, because the operator asked for that table.
    let wide = config.tables.is_empty();
    let mut tables: Tables = BTreeMap::new();
    let mut problems: BTreeMap<String, String> = BTreeMap::new();
    for row in rows.values {
        let table = field(&row, 0)?.to_string();
        if problems.contains_key(&table) {
            continue;
        }
        let column = (|| -> Result<Column> {
            super::settings::validate_identifier("table", &table)?;
            anyhow::ensure!(
                !transactional || field(&row, 6)? == "InnoDB",
                "native MySQL coordinated snapshots and whole transactions require InnoDB"
            );
            let charset = row
                .get(5)
                .context("missing charset column")?
                .as_ref()
                .map(|value| std::str::from_utf8(value).map(str::to_string))
                .transpose()?;
            if !standalone {
                if let Some(charset) = charset.as_deref().filter(|name| *name != "binary") {
                    crate::decoder::charset_encoding(charset)?;
                }
            }
            let column = Column {
                name: field(&row, 1)?.to_string(),
                sql_type: field(&row, 2)?.to_string(),
                key: field(&row, 3)? == "PRI",
                nullable: field(&row, 4)? == "YES",
                charset,
                collation: row
                    .get(7)
                    .context("missing collation column")?
                    .as_ref()
                    .map(|value| -> Result<u16> { Ok(std::str::from_utf8(value)?.parse()?) })
                    .transpose()?,
            };
            wire_types(&column.sql_type)?;
            Ok(column)
        })();
        match column {
            Ok(column) => {
                let columns = tables.entry(table.clone()).or_default();
                anyhow::ensure!(
                    columns.len() < 4096,
                    "MySQL column count exceeds supported limit"
                );
                columns.push(column);
            }
            Err(error) if wide && !standalone => {
                tables.remove(&table);
                problems.insert(table, format!("{error:#}"));
            }
            Err(error) => return Err(error.context(format!("MySQL table {table}"))),
        }
    }
    anyhow::ensure!(
        !require_existing
            || config.tables.is_empty()
            || (tables.len() == config.tables.len()
                && config.tables.iter().all(|table| tables.contains_key(table))),
        "each selected MySQL table must exist"
    );
    for (table, columns) in tables.clone() {
        let keys = if positional {
            positional_keys(config, &table, columns.len())
        } else {
            key_columns(config, &table, &columns)
        };
        let checked = keys.and_then(|keys| {
            anyhow::ensure!(
                positional
                    || keys
                        .iter()
                        .all(|key| columns.iter().any(|column| &column.name == key)),
                "native MySQL key columns must exist"
            );
            Ok(())
        });
        if let Err(error) = checked {
            if wide && !standalone {
                tables.remove(&table);
                problems.insert(table, format!("{error:#}"));
            } else {
                return Err(error.context(format!("MySQL table {table}")));
            }
        }
    }
    anyhow::ensure!(
        !require_existing
            || config.tables.is_empty()
            || config
                .table_keys
                .iter()
                .all(|key| tables.contains_key(&key.table)),
        "configured MySQL key table is missing"
    );
    if !tables.is_empty() {
        let prefixes = connection
            .query(&format!(
                "SELECT TABLE_NAME FROM information_schema.STATISTICS WHERE TABLE_SCHEMA={database} \
            AND TABLE_NAME IN ({}) AND INDEX_NAME='PRIMARY' AND SUB_PART IS NOT NULL",
                text_list(tables.keys())
            ))
            .await?;
        for row in &prefixes.values {
            let table = field(row, 0)?.to_string();
            if config.table_keys.iter().any(|keys| keys.table == table) {
                continue;
            }
            if wide && !standalone {
                tables.remove(&table);
                problems.insert(
                    table,
                    "native MySQL requires full-column primary keys or explicit tableKeys".into(),
                );
            } else {
                anyhow::bail!(
                    "native MySQL requires full-column primary keys (table {table}); configure tableKeys"
                );
            }
        }
    }
    for (table, problem) in &problems {
        log::warn!("Native MySQL skips table {table} in its database-wide selection: {problem}");
    }
    Ok((tables, problems.into_keys().collect()))
}

/// The facts a committed cursor is bound to for one table: its key columns
/// and their declared types, in key order.
pub fn key_facts(
    config: &MySqlConfig,
    table: &str,
    columns: &[Column],
    positional: bool,
) -> Result<Vec<(String, String)>> {
    let keys = if positional {
        positional_keys(config, table, columns.len())?
    } else {
        key_columns(config, table, columns)?
    };
    keys.into_iter()
        .map(|key| {
            let column = if positional {
                key.strip_prefix('@')
                    .and_then(|index| index.parse::<usize>().ok())
                    .and_then(|index| columns.get(index))
            } else {
                columns.iter().find(|column| column.name == key)
            }
            .with_context(|| format!("MySQL key column {key} of {table} is missing"))?;
            Ok((key, column.sql_type.clone()))
        })
        .collect()
}

pub fn key_columns(config: &MySqlConfig, table: &str, columns: &[Column]) -> Result<Vec<String>> {
    if let Some(keys) = config.table_keys.iter().find(|key| key.table == table) {
        return Ok(keys.key_columns.clone());
    }
    // Identity comes only from a declared primary key or tableKeys; a column
    // name such as `id` is never taken as proof of uniqueness.
    let keys = columns
        .iter()
        .filter(|column| column.key)
        .map(|column| column.name.clone())
        .collect::<Vec<_>>();
    anyhow::ensure!(
        !keys.is_empty(),
        "MySQL table {table} has no primary key; add a tableKeys entry"
    );
    Ok(keys)
}

pub fn positional_keys(
    config: &MySqlConfig,
    table: &str,
    column_count: usize,
) -> Result<Vec<String>> {
    let keys = &config
        .table_keys
        .iter()
        .find(|keys| keys.table == table)
        .with_context(|| format!("positional MySQL table {table} requires explicit @N keys"))?
        .key_columns;
    anyhow::ensure!(!keys.is_empty(), "positional MySQL keys are empty");
    for key in keys {
        let index: usize = key
            .strip_prefix('@')
            .context("positional MySQL keys must use @N column offsets")?
            .parse()?;
        anyhow::ensure!(
            index < column_count && key == &format!("@{index}"),
            "invalid positional MySQL key offset"
        );
    }
    Ok(keys.clone())
}

pub fn validate_positional_row(columns: &[Column], row: &BinlogRow) -> Result<()> {
    anyhow::ensure!(
        row.len() == columns.len() && row.columns_ref().len() == columns.len(),
        "MySQL positional column count changed"
    );
    for (index, (expected, actual)) in columns.iter().zip(row.columns_ref()).enumerate() {
        anyhow::ensure!(
            wire_types(&expected.sql_type)?.contains(&actual.column_type()),
            "MySQL positional column {index} type changed; explicit schema recovery is required"
        );
        let value = row
            .as_ref(index)
            .context("incomplete MySQL positional row")?;
        anyhow::ensure!(
            expected.nullable
                || !matches!(
                    value,
                    mysql_common::binlog::value::BinlogValue::Value(mysql_common::Value::NULL)
                ),
            "NULL in a non-null MySQL positional column"
        );
    }
    Ok(())
}

fn validate_retention(rows: &[Vec<Option<Vec<u8>>>]) -> Result<()> {
    let mut settings = BTreeMap::new();
    for row in rows {
        anyhow::ensure!(
            settings.insert(field(row, 0)?, field(row, 1)?).is_none(),
            "duplicate MySQL retention setting"
        );
    }
    let seconds: u64 = settings
        .get("binlog_expire_logs_seconds")
        .context("missing MySQL binlog retention setting")?
        .parse()?;
    let days = settings
        .get("expire_logs_days")
        .map(|value| value.parse::<u64>())
        .transpose()?;
    let disabled = match settings.get("binlog_expire_logs_auto_purge").copied() {
        Some("OFF") => true,
        Some("ON") | None => seconds == 0 && days.is_none_or(|days| days == 0),
        Some(_) => anyhow::bail!("invalid MySQL automatic binlog purge setting"),
    };
    anyhow::ensure!(
        disabled,
        "retention until processing requires automatic MySQL binlog expiry disabled"
    );
    Ok(())
}

pub fn validate_row(columns: &[Column], row: &BinlogRow) -> Result<()> {
    anyhow::ensure!(
        row.len() == columns.len() && row.columns_ref().len() == columns.len(),
        "MySQL column count changed"
    );
    for (index, (expected, actual)) in columns.iter().zip(row.columns_ref()).enumerate() {
        use mysql_common::{binlog::value::BinlogValue, Value};
        let sql_type = expected
            .sql_type
            .split(['(', ' '])
            .next()
            .context("missing SQL type")?;
        let compatible = wire_types(&expected.sql_type)?.contains(&actual.column_type());
        let unsigned = sql_type == "year"
            || (!matches!(sql_type, "enum" | "set")
                && expected
                    .sql_type
                    .split_whitespace()
                    .any(|word| word == "unsigned"));
        let binary = matches!(
            sql_type,
            "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob"
        );
        let collation = expected.collation.or(binary.then_some(63));
        anyhow::ensure!(
            compatible && expected.name.as_bytes() == actual.name_ref()
                && collation.is_none_or(|collation| collation == actual.character_set())
                && expected.key == actual.flags().contains(ColumnFlags::PRI_KEY_FLAG)
                && unsigned == actual.flags().contains(ColumnFlags::UNSIGNED_FLAG),
            "MySQL column {} identity, type, charset, key or signedness changed: expected {sql_type}, charset {collation:?}, key {}, unsigned {unsigned}; got {} {:?}, charset {}, key {}, unsigned {}; explicit schema recovery is required",
            expected.name, expected.key, actual.name_str(), actual.column_type(),
            actual.character_set(), actual.flags().contains(ColumnFlags::PRI_KEY_FLAG),
            actual.flags().contains(ColumnFlags::UNSIGNED_FLAG)
        );
        let value = row.as_ref(index).context("incomplete MySQL row")?;
        anyhow::ensure!(
            expected.nullable || !matches!(value, BinlogValue::Value(Value::NULL)),
            "NULL in non-null MySQL column"
        );
    }
    Ok(())
}

pub fn validate_snapshot_columns(
    expected: &[Column],
    actual: &[mysql_common::packets::Column],
) -> Result<()> {
    use mysql_common::constants::ColumnType::*;
    anyhow::ensure!(
        expected.len() == actual.len(),
        "MySQL snapshot column count changed"
    );
    for (expected, actual) in expected.iter().zip(actual) {
        let sql_type = expected
            .sql_type
            .split(['(', ' '])
            .next()
            .context("missing SQL type")?;
        let compatible = match sql_type {
            "enum" => {
                actual.column_type() == MYSQL_TYPE_STRING
                    && actual.flags().contains(ColumnFlags::ENUM_FLAG)
            }
            "set" => {
                actual.column_type() == MYSQL_TYPE_STRING
                    && actual.flags().contains(ColumnFlags::SET_FLAG)
            }
            "tinyblob" | "tinytext" => {
                matches!(actual.column_type(), MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_BLOB)
            }
            "mediumblob" | "mediumtext" => matches!(
                actual.column_type(),
                MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_BLOB
            ),
            "longblob" | "longtext" => {
                matches!(actual.column_type(), MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB)
            }
            _ => wire_types(&expected.sql_type)?.contains(&actual.column_type()),
        };
        let unsigned = matches!(sql_type, "year" | "bit")
            || (!matches!(sql_type, "enum" | "set")
                && expected
                    .sql_type
                    .split_whitespace()
                    .any(|word| word == "unsigned"));
        anyhow::ensure!(
            compatible
                && expected.name.as_bytes() == actual.name_ref()
                && unsigned == actual.flags().contains(ColumnFlags::UNSIGNED_FLAG),
            "MySQL snapshot column {} identity, type or signedness changed: expected {sql_type}, unsigned {unsigned}; got {} {:?}, unsigned {}",
            expected.name, actual.name_str(), actual.column_type(),
            actual.flags().contains(ColumnFlags::UNSIGNED_FLAG)
        );
    }
    Ok(())
}

fn wire_types(sql_type: &str) -> Result<&'static [mysql_common::constants::ColumnType]> {
    use mysql_common::constants::ColumnType::*;
    Ok(
        match sql_type
            .split(['(', ' '])
            .next()
            .context("missing SQL type")?
        {
            "tinyint" => &[MYSQL_TYPE_TINY],
            "smallint" => &[MYSQL_TYPE_SHORT],
            "mediumint" => &[MYSQL_TYPE_INT24],
            "int" => &[MYSQL_TYPE_LONG],
            "bigint" => &[MYSQL_TYPE_LONGLONG],
            "float" => &[MYSQL_TYPE_FLOAT],
            "double" => &[MYSQL_TYPE_DOUBLE],
            "decimal" => &[MYSQL_TYPE_NEWDECIMAL],
            "year" => &[MYSQL_TYPE_YEAR],
            "date" => &[MYSQL_TYPE_DATE, MYSQL_TYPE_NEWDATE],
            "time" => &[MYSQL_TYPE_TIME, MYSQL_TYPE_TIME2],
            "datetime" => &[MYSQL_TYPE_DATETIME, MYSQL_TYPE_DATETIME2],
            "timestamp" => &[MYSQL_TYPE_TIMESTAMP, MYSQL_TYPE_TIMESTAMP2],
            "varchar" | "varbinary" => &[MYSQL_TYPE_VARCHAR, MYSQL_TYPE_VAR_STRING],
            "char" | "binary" => &[MYSQL_TYPE_STRING],
            "tinytext" | "text" | "mediumtext" | "longtext" | "tinyblob" | "blob"
            | "mediumblob" | "longblob" => &[MYSQL_TYPE_BLOB],
            "bit" => &[MYSQL_TYPE_BIT],
            "enum" => &[MYSQL_TYPE_ENUM],
            "set" => &[MYSQL_TYPE_SET],
            "json" => &[MYSQL_TYPE_JSON],
            "geometry" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring"
            | "multipolygon" | "geometrycollection" | "geomcollection" => &[MYSQL_TYPE_GEOMETRY],
            "vector" => &[MYSQL_TYPE_VECTOR],
            _ => anyhow::bail!("unsupported native MySQL column type {sql_type}"),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_declared_never_inferred_from_an_id_column() {
        let column = |name: &str, key: bool| Column {
            name: name.into(),
            sql_type: "int".into(),
            key,
            nullable: false,
            charset: None,
            collation: None,
        };
        let mut config = super::super::tests::config(3306, super::super::MySqlOutput::Changes);
        let keyless = [column("id", false), column("value", false)];
        let error = key_columns(&config, "items", &keyless).unwrap_err();
        assert!(error.to_string().contains("tableKeys"), "{error:#}");
        let keyed = [column("id", false), column("code", true)];
        assert_eq!(key_columns(&config, "items", &keyed).unwrap(), ["code"]);
        config.table_keys.push(crate::TableKeyConfig {
            table: "items".into(),
            key_columns: vec!["id".into()],
        });
        assert_eq!(key_columns(&config, "items", &keyless).unwrap(), ["id"]);
    }

    #[test]
    fn retention_disables_both_legacy_and_current_expiry_settings() {
        let validate = |values: &[(&str, &str)]| {
            let rows = values
                .iter()
                .map(|(name, value)| {
                    vec![
                        Some(name.as_bytes().to_vec()),
                        Some(value.as_bytes().to_vec()),
                    ]
                })
                .collect::<Vec<_>>();
            validate_retention(&rows)
        };
        assert!(validate(&[]).is_err());
        assert!(validate(&[("binlog_expire_logs_seconds", "0")]).is_ok());
        assert!(validate(&[
            ("binlog_expire_logs_seconds", "0"),
            ("expire_logs_days", "1")
        ])
        .is_err());
        assert!(validate(&[
            ("binlog_expire_logs_seconds", "3600"),
            ("expire_logs_days", "0")
        ])
        .is_err());
        assert!(validate(&[
            ("binlog_expire_logs_seconds", "0"),
            ("expire_logs_days", "0"),
            ("binlog_expire_logs_auto_purge", "ON")
        ])
        .is_ok());
        assert!(validate(&[
            ("binlog_expire_logs_seconds", "3600"),
            ("expire_logs_days", "0"),
            ("binlog_expire_logs_auto_purge", "OFF")
        ])
        .is_ok());
        assert!(validate(&[
            ("binlog_expire_logs_seconds", "0"),
            ("binlog_expire_logs_auto_purge", "unknown")
        ])
        .is_err());
        assert!(validate(&[
            ("binlog_expire_logs_seconds", "0"),
            ("expire_logs_days", "bad")
        ])
        .is_err());
        assert!(validate(&[
            ("binlog_expire_logs_seconds", "0"),
            ("binlog_expire_logs_seconds", "0")
        ])
        .is_err());
    }
}
