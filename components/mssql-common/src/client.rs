// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! One SQL Server client implementation for the native plugins: transport
//! settings, authentication, connecting (with one Azure SQL redirect) and
//! failure classification by error number or type.
//!
//! The native CDC source, its bootstrap providers and the stored-procedure
//! sink each own their configuration struct and project it into
//! [`ConnectionSettings`], so the TLS, trust and authentication rules are
//! implemented once. Classification never inspects message text.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use tiberius::{AuthMethod, Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

/// A Tiberius client over a Tokio TCP stream.
pub type SqlClient = Client<Compat<TcpStream>>;

/// Largest accepted `trustedCaPem` bundle.
pub const MAX_PEM_BYTES: usize = 256 * 1024;
/// Longest accepted host name.
pub const MAX_HOST_BYTES: usize = 253;
/// Longest SQL Server identifier (database names included).
pub const MAX_IDENTIFIER_CHARS: usize = 128;
/// Longest accepted login or password.
pub const MAX_CREDENTIAL_BYTES: usize = 1024;

/// TDS encryption. `required` (the default) encrypts the whole session and
/// verifies the server certificate. The other modes are explicit opt-ins to
/// plaintext; callers log a warning at start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Encryption {
    /// TLS for the whole session.
    #[default]
    Required,
    /// TLS for the login packet only; query data is plaintext.
    LoginOnly,
    /// No TLS at all; credentials and data are plaintext.
    Disabled,
}

impl Encryption {
    /// The Tiberius encryption level for this mode.
    pub fn level(self) -> tiberius::EncryptionLevel {
        match self {
            Self::Required => tiberius::EncryptionLevel::Required,
            Self::LoginOnly => tiberius::EncryptionLevel::Off,
            Self::Disabled => tiberius::EncryptionLevel::NotSupported,
        }
    }
}

/// How a session authenticates. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub enum Authentication {
    /// SQL Server authentication.
    Sql { user: String, password: String },
    /// A Microsoft Entra ID access token for `https://database.windows.net/`
    /// (Tiberius `AADToken`).
    AccessToken { token: String },
}

impl std::fmt::Debug for Authentication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Sql { .. } => "Sql([REDACTED])",
            Self::AccessToken { .. } => "AccessToken([REDACTED])",
        })
    }
}

impl Authentication {
    /// The Tiberius authentication method.
    pub fn method(&self) -> AuthMethod {
        match self {
            Self::Sql { user, password } => AuthMethod::sql_server(user, password),
            Self::AccessToken { token } => AuthMethod::aad_token(token),
        }
    }
}

/// The transport part of a native SQL Server configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionSettings<'a> {
    pub host: &'a str,
    pub port: u16,
    pub database: &'a str,
    pub encryption: Encryption,
    pub trust_server_certificate: bool,
    pub trusted_ca_pem: Option<&'a str>,
    pub host_name_in_certificate: Option<&'a str>,
    /// Per-round-trip deadline Tiberius applies to each command.
    pub command_timeout: Duration,
    /// TCP, TLS and login deadline of one connection attempt.
    pub connect_timeout: Duration,
    /// Reported to the server as the application name.
    pub application_name: &'a str,
}

impl ConnectionSettings<'_> {
    /// Field checks shared by every native configuration.
    pub fn validate(&self) -> anyhow::Result<()> {
        use anyhow::ensure;
        ensure!(
            !self.host.is_empty() && self.host.len() <= MAX_HOST_BYTES && !self.host.contains('\0'),
            "host must be a nonempty host name of at most {MAX_HOST_BYTES} bytes"
        );
        ensure!(self.port != 0, "port must be in 1..=65535");
        ensure!(
            !self.database.is_empty()
                && self.database.chars().count() <= MAX_IDENTIFIER_CHARS
                && !self.database.contains('\0'),
            "database must be a nonempty name of at most {MAX_IDENTIFIER_CHARS} characters"
        );
        if let Some(pem) = self.trusted_ca_pem {
            ensure!(
                pem.len() <= MAX_PEM_BYTES && pem.contains("-----BEGIN CERTIFICATE-----"),
                "trustedCaPem must hold PEM certificates of at most {MAX_PEM_BYTES} bytes"
            );
            ensure!(
                !self.trust_server_certificate,
                "trustServerCertificate and trustedCaPem are mutually exclusive"
            );
        }
        if self.encryption == Encryption::Disabled {
            ensure!(
                !self.trust_server_certificate
                    && self.trusted_ca_pem.is_none()
                    && self.host_name_in_certificate.is_none(),
                "certificate settings require encryption"
            );
        }
        if let Some(name) = self.host_name_in_certificate {
            ensure!(
                !name.is_empty() && name.len() <= MAX_HOST_BYTES && !name.contains('\0'),
                "hostNameInCertificate must be a nonempty host name"
            );
        }
        Ok(())
    }

    /// Whether query data travels in plaintext.
    pub fn plaintext(&self) -> bool {
        self.encryption != Encryption::Required
    }

    /// Logs the transport opt-ins that weaken security. `component` names
    /// the plugin in the message.
    pub fn warn_about_transport(&self, component: &str) {
        if self.plaintext() {
            log::warn!(
                "{component} uses encryption={:?}: query data{} travel in plaintext. \
                 Use encryption=required.",
                self.encryption,
                if self.encryption == Encryption::Disabled {
                    " and credentials"
                } else {
                    ""
                }
            );
        }
        if self.trust_server_certificate {
            log::warn!(
                "{component} does not validate the server certificate \
                 (trustServerCertificate=true); use trustedCaPem instead outside tests."
            );
        }
    }

    /// The Tiberius configuration. `validate` makes the trust settings
    /// mutually exclusive (Tiberius panics otherwise).
    pub fn tiberius_config(&self, authentication: &Authentication) -> Config {
        let mut tiberius = Config::new();
        tiberius.host(self.host);
        tiberius.port(self.port);
        tiberius.database(self.database);
        tiberius.application_name(self.application_name);
        tiberius.encryption(self.encryption.level());
        if self.trust_server_certificate {
            tiberius.trust_cert();
        } else if let Some(pem) = self.trusted_ca_pem {
            tiberius.trust_cert_ca_bundle(pem.as_bytes().to_vec());
        }
        if let Some(name) = self.host_name_in_certificate {
            tiberius.hostname_in_certificate(name);
        }
        tiberius.command_timeout(Some(self.command_timeout));
        tiberius.handshake_timeout(Some(self.connect_timeout));
        tiberius.authentication(authentication.method());
        tiberius
    }
}

/// Checks literal credentials for construction: a login and a nonempty
/// password are required unless the host binds an activation identity.
pub fn validate_literal_credentials(
    user: &str,
    password: Option<&str>,
    identity_bound: bool,
) -> anyhow::Result<()> {
    use anyhow::ensure;
    ensure!(
        user.len() <= MAX_CREDENTIAL_BYTES && !user.contains('\0'),
        "user must be at most {MAX_CREDENTIAL_BYTES} bytes"
    );
    if let Some(password) = password {
        ensure!(
            password.len() <= MAX_CREDENTIAL_BYTES && !password.contains('\0'),
            "password must be at most {MAX_CREDENTIAL_BYTES} bytes"
        );
    }
    if !identity_bound {
        ensure!(
            !user.is_empty() && password.is_some_and(|p| !p.is_empty()),
            "user and password are required unless an activation identity is bound"
        );
    }
    Ok(())
}

/// Opens TCP, negotiates TLS and logs in. A login redirect (Azure SQL
/// gateway routing) is followed once. Errors keep their Tiberius type so
/// callers can classify them. The caller bounds the whole call.
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

/// [`connect`] bounded by `timeout`; an expiry is a [`ConnectTimeout`].
pub async fn connect_within(config: Config, timeout: Duration) -> anyhow::Result<SqlClient> {
    use anyhow::Context;
    tokio::time::timeout(timeout, connect(config))
        .await
        .map_err(|_| ConnectTimeout(timeout))?
        .context("connecting to SQL Server")
}

/// A connection attempt exceeded its connect timeout. Retryable.
#[derive(Debug)]
pub struct ConnectTimeout(pub Duration);

impl std::fmt::Display for ConnectTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SQL Server connection timed out after {:?}", self.0)
    }
}

impl std::error::Error for ConnectTimeout {}

/// How a failure is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The same operation may succeed later. `reconnect` drops the
    /// connection first (it is broken or its state is unknown).
    Retryable { reconnect: bool },
    /// Retrying gives the same answer.
    Permanent,
}

impl FailureClass {
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Retryable { .. })
    }
}

/// SQL Server errors after which the transaction was rolled back but the
/// connection stays usable.
pub const TRANSIENT_SERVER_ERRORS: [u32; 8] = [
    1205,  // deadlock victim
    1222,  // lock request timeout
    3960,  // snapshot isolation update conflict
    3961,  // snapshot isolation: object changed by DDL
    41301, // in-memory OLTP dependency failure
    41302, // in-memory OLTP update conflict
    41305, // in-memory OLTP repeatable-read validation
    41325, // in-memory OLTP serializable validation
];

/// SQL Server and Azure SQL errors after which the connection is closed or
/// should be replaced (failover, throttling, service busy, network).
pub const TRANSIENT_CONNECTION_ERRORS: [u32; 12] = [
    233, 64, 4221, 10053, 10054, 10060, 10928, 10929, 40197, 40501, 40613, 49918,
];

/// Classifies a SQL Server error number. Everything not listed (procedure
/// `THROW`/`RAISERROR`, constraint violations, permissions, login failures,
/// syntax) is permanent.
pub fn classify_server_code(code: u32) -> FailureClass {
    if TRANSIENT_SERVER_ERRORS.contains(&code) {
        FailureClass::Retryable { reconnect: false }
    } else if TRANSIENT_CONNECTION_ERRORS.contains(&code) || (49919..=49920).contains(&code) {
        FailureClass::Retryable { reconnect: true }
    } else {
        FailureClass::Permanent
    }
}

/// Classifies one error in a chain, or `None` when its type says nothing.
pub fn classify_cause(cause: &(dyn std::error::Error + 'static)) -> Option<FailureClass> {
    if let Some(error) = cause.downcast_ref::<tiberius::error::Error>() {
        return Some(classify_tiberius(error));
    }
    if cause.downcast_ref::<std::io::Error>().is_some()
        || cause
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
        || cause.downcast_ref::<ConnectTimeout>().is_some()
    {
        return Some(FailureClass::Retryable { reconnect: true });
    }
    None
}

/// Classifies by type, SQL Server error number or I/O kind, never by text.
/// The first cause with a known type decides; unknown errors are permanent.
pub fn classify(error: &anyhow::Error) -> FailureClass {
    error
        .chain()
        .find_map(classify_cause)
        .unwrap_or(FailureClass::Permanent)
}

fn classify_tiberius(error: &tiberius::error::Error) -> FailureClass {
    use tiberius::error::Error;
    match error {
        Error::Server(token) => classify_server_code(token.code()),
        // A broken or desynchronised connection: the server rolls back the
        // open transaction when it closes, so a retry after reconnect is safe.
        Error::Io { .. } | Error::Protocol(_) | Error::Routing { .. } => {
            FailureClass::Retryable { reconnect: true }
        }
        _ => FailureClass::Permanent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> ConnectionSettings<'static> {
        ConnectionSettings {
            host: "sql.example",
            port: 1433,
            database: "app",
            encryption: Encryption::Required,
            trust_server_certificate: false,
            trusted_ca_pem: None,
            host_name_in_certificate: None,
            command_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(15),
            application_name: "test",
        }
    }

    #[test]
    fn encryption_defaults_to_required_and_maps_to_tiberius() {
        assert_eq!(Encryption::default(), Encryption::Required);
        assert_eq!(
            Encryption::Required.level(),
            tiberius::EncryptionLevel::Required
        );
        assert_eq!(
            Encryption::LoginOnly.level(),
            tiberius::EncryptionLevel::Off
        );
        assert_eq!(
            Encryption::Disabled.level(),
            tiberius::EncryptionLevel::NotSupported
        );
        assert_eq!(
            serde_json::to_value(Encryption::LoginOnly).unwrap(),
            serde_json::json!("loginOnly")
        );
    }

    #[test]
    fn transport_rules_reject_conflicting_trust_and_plaintext_certificates() {
        settings().validate().unwrap();
        assert!(!settings().plaintext());
        let pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
        let mut both = settings();
        both.trusted_ca_pem = Some(pem);
        both.trust_server_certificate = true;
        assert!(both.validate().is_err());
        let mut plaintext = settings();
        plaintext.encryption = Encryption::Disabled;
        plaintext.trust_server_certificate = true;
        assert!(plaintext.validate().is_err());
        for (host, database) in [("", "d"), ("h", ""), ("h\0", "d")] {
            let mut invalid = settings();
            invalid.host = host;
            invalid.database = database;
            assert!(invalid.validate().is_err(), "{host:?} {database:?}");
        }
        let mut port = settings();
        port.port = 0;
        assert!(port.validate().is_err());
        let _ = settings().tiberius_config(&Authentication::AccessToken { token: "t".into() });
    }

    #[test]
    fn credentials_are_required_without_an_identity_and_never_printed() {
        assert!(validate_literal_credentials("", None, false).is_err());
        assert!(validate_literal_credentials("u", Some(""), false).is_err());
        validate_literal_credentials("u", Some("p"), false).unwrap();
        validate_literal_credentials("", None, true).unwrap();
        assert!(validate_literal_credentials("u\0", Some("p"), true).is_err());
        let sql = Authentication::Sql {
            user: "u".into(),
            password: "hunter2".into(),
        };
        assert!(!format!("{sql:?}").contains("hunter2"));
        let token = Authentication::AccessToken {
            token: "eyJ".into(),
        };
        assert!(!format!("{token:?}").contains("eyJ"));
    }

    #[test]
    fn errors_are_classified_by_number_and_type_never_by_text() {
        for code in TRANSIENT_SERVER_ERRORS {
            assert_eq!(
                classify_server_code(code),
                FailureClass::Retryable { reconnect: false }
            );
        }
        for code in TRANSIENT_CONNECTION_ERRORS
            .into_iter()
            .chain([49919, 49920])
        {
            assert_eq!(
                classify_server_code(code),
                FailureClass::Retryable { reconnect: true }
            );
        }
        for code in [50000, 547, 2627, 229, 262, 18456, 208, 102] {
            assert_eq!(classify_server_code(code), FailureClass::Permanent);
        }
        let io = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert!(classify(&io.context("polling")).is_retryable());
        assert!(
            classify(&anyhow::Error::new(ConnectTimeout(Duration::from_secs(1)))).is_retryable()
        );
        assert_eq!(
            classify(&anyhow::Error::from(tiberius::error::Error::Tls(
                "bad certificate".into()
            ))),
            FailureClass::Permanent
        );
        assert_eq!(
            classify(&anyhow::anyhow!("deadlock victim 1205 connection reset")),
            FailureClass::Permanent
        );
    }
}
