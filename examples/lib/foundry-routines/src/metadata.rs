// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use crate::config::Connection;
use anyhow::{Context, Result};
use mysql_async::prelude::Queryable;
use serde_json::{json, Value};

pub async fn postgres(
    config: &Connection,
    password: &str,
) -> Result<(
    tokio_postgres::Client,
    tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>,
)> {
    let mut options = tokio_postgres::Config::new();
    options
        .host(&config.host)
        .port(config.port)
        .dbname(&config.database)
        .user(&config.user)
        .password(password)
        .connect_timeout(std::time::Duration::from_secs(10));
    if config.tls == "disabled" {
        options.ssl_mode(tokio_postgres::config::SslMode::Disable);
        let (client, connection) = options.connect(tokio_postgres::NoTls).await?;
        Ok((client, tokio::spawn(connection)))
    } else {
        options.ssl_mode(tokio_postgres::config::SslMode::Require);
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let (client, connection) = options
            .connect(tokio_postgres_rustls::MakeRustlsConnect::new(tls))
            .await?;
        Ok((client, tokio::spawn(connection)))
    }
}

pub async fn mysql(config: &Connection, password: &str) -> Result<mysql_async::Conn> {
    let mode = if config.tls == "disabled" {
        drasi_mysql_common::config::SslMode::Disabled
    } else {
        drasi_mysql_common::config::SslMode::RequireVerifyFull
    };
    let options = mysql_async::OptsBuilder::default()
        .ip_or_hostname(config.host.clone())
        .tcp_port(config.port)
        .db_name(Some(config.database.clone()))
        .user(Some(config.user.clone()))
        .pass(Some(password.to_owned()));
    drasi_mysql_common::connect_with_ssl_mode(|| options.clone(), mode).await
}

pub async fn validate(config: &Connection) -> Result<Value> {
    let password = config.password()?;
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        inspect(config, &password),
    )
    .await
    .context("Database metadata check timed out")?
}

async fn inspect(config: &Connection, password: &str) -> Result<Value> {
    let mut tables = Vec::new();
    let mut checks = vec![
        json!({"name":"Connection","ok":true,"detail":"Authenticated read-only metadata connection"}),
    ];
    if config.kind == "postgres" {
        let (client, driver) = postgres(config, password).await?;
        let result = async {
            let rows = client.query("SELECT table_schema,table_name FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog','information_schema') AND table_type = 'BASE TABLE' ORDER BY table_schema,table_name",&[]).await?;
            for row in rows {
                let schema: String = row.get(0);
                let name: String = row.get(1);
                let label=if schema=="public" {name.clone()} else {format!("{schema}.{name}")};
                let columns = client.query("SELECT c.column_name, c.data_type, EXISTS (SELECT 1 FROM information_schema.table_constraints t JOIN information_schema.key_column_usage k ON t.constraint_name=k.constraint_name AND t.table_schema=k.table_schema WHERE t.constraint_type='PRIMARY KEY' AND t.table_schema=c.table_schema AND t.table_name=c.table_name AND k.column_name=c.column_name) FROM information_schema.columns c WHERE c.table_schema=$1 AND c.table_name=$2 ORDER BY c.ordinal_position",&[&schema,&name]).await?;
                tables.push(json!({"name":label,"label":label,"columns":columns.iter().map(|r|json!({"name":r.get::<_,String>(0),"type":r.get::<_,String>(1),"primaryKey":r.get::<_,bool>(2)})).collect::<Vec<_>>()}));
            }
            let wal: String = client.query_one("SHOW wal_level",&[]).await?.get(0);
            let replication: bool = client.query_one("SELECT rolreplication OR rolsuper FROM pg_roles WHERE rolname=current_user",&[]).await?.get(0);
            let published = client.query("SELECT schemaname,tablename FROM pg_publication_tables WHERE pubname=$1",&[&config.publication]).await?;
            let published: Vec<String> = published.iter().map(|r| {let schema:String=r.get(0);let name:String=r.get(1);if schema=="public" {name} else {format!("{schema}.{name}")}}).collect();
            checks.push(json!({"name":"Logical replication","ok":wal=="logical","detail":format!("wal_level = {wal}")}));
            checks.push(json!({"name":"Replication privilege","ok":replication,"detail":"The source user requires REPLICATION"}));
            checks.push(json!({"name":"Publication","ok":!published.is_empty() && config.tables.iter().all(|t|published.contains(t)),"detail":format!("Publication '{}' must include selected tables",config.publication)}));
            Ok::<_,anyhow::Error>(())
        }.await;
        drop(client);
        driver
            .await
            .context("PostgreSQL metadata worker failed")??;
        result?;
    } else {
        let mut client = mysql(config, password).await?;
        let rows: Vec<(String,)> = client.exec("SELECT table_name FROM information_schema.tables WHERE table_schema=? AND table_type='BASE TABLE' ORDER BY table_name",(&config.database,)).await?;
        for (name,) in rows {
            let columns: Vec<(String,String,String)> = client.exec("SELECT column_name,data_type,column_key FROM information_schema.columns WHERE table_schema=? AND table_name=? ORDER BY ordinal_position",(&config.database,&name)).await?;
            tables.push(json!({"name":name,"label":name,"columns":columns.into_iter().map(|(name,kind,key)|json!({"name":name,"type":kind,"primaryKey":key=="PRI"})).collect::<Vec<_>>()}));
        }
        let (enabled,format,image,metadata): (u8,String,String,String) = client.query_first("SELECT @@global.log_bin,@@global.binlog_format,@@global.binlog_row_image,@@global.binlog_row_metadata").await?.context("MySQL replication settings are unavailable")?;
        let grants: Vec<String> = client.query("SHOW GRANTS FOR CURRENT_USER()").await?;
        let replication = grants
            .iter()
            .any(|g| g.starts_with("GRANT ALL PRIVILEGES ON *.*"))
            || (grants.iter().any(|g| g.contains("REPLICATION SLAVE"))
                && grants.iter().any(|g| g.contains("REPLICATION CLIENT")));
        checks.push(json!({"name":"Row binlog","ok":enabled==1 && format=="ROW" && image=="FULL" && metadata=="FULL","detail":format!("log_bin={enabled}, format={format}, row_image={image}, row_metadata={metadata}")}));
        checks.push(json!({"name":"Replication privilege","ok":replication,"detail":"REPLICATION SLAVE and REPLICATION CLIENT are required"}));
        client.disconnect().await?;
    }
    Ok(json!({"tables":tables,"checks":checks}))
}

pub async fn execute(config: &Connection, password: &str, sql: &str) -> Result<()> {
    if config.kind == "postgres" {
        let (client, driver) = postgres(config, password).await?;
        let result = client.batch_execute(sql).await;
        drop(client);
        driver.await??;
        result?;
    } else {
        let mut client = mysql(config, password).await?;
        client.query_drop(sql).await?;
        client.disconnect().await?;
    }
    Ok(())
}
