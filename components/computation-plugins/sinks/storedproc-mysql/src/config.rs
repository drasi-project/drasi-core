// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Published configuration of `drasi.mysql/storedproc`.

pub use drasi_mysql_common::storedproc::config::*;
use drasi_mysql_common::storedproc::delivery::{DEFAULT_MAX_STREAMS, DEFAULT_PROGRESS_TABLE};

/// Field descriptions, defaults and ranges published to hosts.
pub fn definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
    use drasi_computation_plugin_sdk::{ConfigDefinition, ConfigOption as Field, ConfigType::*};
    use serde_json::json;
    let template = "An object with optional added, updated and deleted entries, each {\"template\": \"CALL proc({{param after.id}})\"}. Every value must use {{param <path>}}, which binds a positional ? parameter; plain {{x}}, raw {{{x}}}, {{json x}}, partials and decorators are rejected. if, unless, each and with may select or exclude a command.";
    // Constant declarations; `metadata_is_valid` proves they are accepted.
    ConfigDefinition::try_new([
        Field::new("hostname", String, false, "MySQL server host name or address.").default("localhost"),
        Field::new("port", Integer, false, "MySQL server port.").default(DEFAULT_PORT).minimum(1).maximum(65_535),
        Field::new("database", String, true, "Database that holds the procedures and the delivery progress table (letters, digits and underscores).").default_description("Required; no default."),
        Field::new("user", String, false, "MySQL account. Required unless the component binds activation credentials (an identity provider), which then supply the account at each Start.").default_description("Required unless activation credentials are bound."),
        Field::new("password", String, false, "Password for user. Ignored when activation credentials are bound.").secret().default_description("Empty."),
        Field::new("ssl", Boolean, false, "Require TLS and verify the server certificate chain and host name (verify-full).").default(false),
        Field::new("tlsCaPem", String, false, "PEM CA certificate trusted in addition to the system roots; requires ssl: true (at most 64 KiB)."),
        Field::new("outputs", Object, false, "Optional aliases for incoming outputs: a map of alias to {component, port?}. Every incoming graph edge is accepted whether or not it is aliased; an unaliased output is named after its producing component. Routes are keyed by alias.").constraint(drasi_computation_contracts::ConfigurationConstraint::output_aliases()),
        Field::new("routes", Object, false, &format!("Per-output-alias command templates. {template} A route key matches an alias or its last dotted segment.")).default(json!({})),
        Field::new("defaultTemplate", Object, false, &format!("Command templates used when no route matches. {template}")),
        Field::new("progressTable", String, false, "Destination cursor table in database (format version 2, shared by the stored-procedure sinks). Created at Start when absent and the account may CREATE; otherwise it must be pre-created.").default(DEFAULT_PROGRESS_TABLE),
        Field::new("maxStreams", Integer, false, "Delivery streams (one per producer output and consumer) this component may hold in the progress table.").default(DEFAULT_MAX_STREAMS).minimum(1).maximum(u64::from(MAX_STREAMS)),
        Field::new("commandTimeoutMs", Integer, false, "Deadline for each SQL statement of an operation, including the procedure call. An expired statement retires (KILLs) the session and rolls the operation back.").default(DEFAULT_COMMAND_TIMEOUT_MS).minimum(1).maximum(MAX_DURATION_MS),
        Field::new("connectTimeoutMs", Integer, false, "Deadline for one connection attempt (TCP, TLS, authentication and session ownership check).").default(DEFAULT_CONNECT_TIMEOUT_MS).minimum(1).maximum(MAX_DURATION_MS),
        Field::new("shutdownTimeoutMs", Integer, false, "Bound on session cleanup in Stop and Dispose when the host gives no deadline, and on retiring a failed session during delivery.").default(DEFAULT_SHUTDOWN_TIMEOUT_MS).minimum(1).maximum(MAX_DURATION_MS),
        Field::new("connectRetryAttempts", Integer, false, "Additional connection attempts after the first, with full-jitter exponential backoff (100 ms to 5 s), for transport failures and server overload. Authentication failures are not retried.").default(DEFAULT_CONNECT_RETRY_ATTEMPTS).minimum(0).maximum(u64::from(MAX_CONNECT_RETRY_ATTEMPTS)),
        Field::new("reconnectBudgetMs", Integer, false, "Total time one connect sequence (attempts and backoff) may take, at Start and when a delivery reconnects.").default(DEFAULT_RECONNECT_BUDGET_MS).minimum(1).maximum(MAX_DURATION_MS),
        Field::new("maxProtocolBytes", Integer, false, "Largest MySQL protocol packet, which bounds one command and its parameters (and any result read).").default(DEFAULT_MAX_PROTOCOL_BYTES as u64).minimum(MIN_PROTOCOL_BYTES as i64).maximum(MAX_PROTOCOL_BYTES as u64),
    ])
    .expect("constant MySQL stored-procedure configuration")
}
