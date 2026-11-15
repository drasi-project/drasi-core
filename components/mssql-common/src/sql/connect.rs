// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Opening a Tiberius client for a prepared [`tiberius::Config`].
//!
//! The caller builds the configuration (authentication, encryption, trust and
//! the per-round-trip command timeout) and bounds the whole call with its own
//! deadline. A login redirect (Azure SQL gateway routing) is followed once.

use tiberius::{Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

/// A Tiberius client over a Tokio TCP stream.
pub type SqlClient = Client<Compat<TcpStream>>;

/// Opens TCP, negotiates TLS and logs in. Errors keep their Tiberius type so
/// callers can classify them by server error number or I/O kind.
pub async fn connect(config: Config) -> Result<SqlClient, tiberius::error::Error> {
    match open(config.clone()).await {
        Err(tiberius::error::Error::Routing { host, port }) => {
            let mut routed = config;
            routed.host(&host);
            routed.port(port);
            open(routed).await
        }
        result => result,
    }
}

async fn open(config: Config) -> Result<SqlClient, tiberius::error::Error> {
    let tcp = TcpStream::connect(config.get_addr()).await?;
    tcp.set_nodelay(true)?;
    Client::connect(config, tcp.compat_write()).await
}
