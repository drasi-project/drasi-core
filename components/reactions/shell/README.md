# Shell Reaction

A Linux-only reaction that invokes a configurable shell command for each query result change, delivering data via stdin and environment variables.

## Overview

The Shell Reaction monitors continuous query results and executes a configured shell command (script) for each change event. Data is passed to the command via stdin as a rendered Handlebars template (or raw JSON when no template is configured), and additional context can be injected as environment variables.

**Platform**: Linux only (`#[cfg(target_os = "linux")]`).

### Key Capabilities

- **Per-Query Command**: Map each subscribed query to its own executable or script
- **Stdin Delivery**: Pass structured data to commands via stdin using Handlebars templates or raw JSON
- **Environment Variable Injection**: Inject rendered template values as environment variables (global or per-operation)
- **Concurrent Execution**: Run multiple commands in parallel, bounded by a size-configurable semaphore
- **Execution Timeout**: Automatically kill commands (and their process group) that exceed a configurable timeout
- **Invocation History**: Retain a rolling window of recent invocation details (exit status, stdout, stderr, timestamps) for observability
- **Metrics**: Expose runtime metrics such as active processes, timeouts, and payload rejections as component properties.

### Use Cases

- **Script Automation**: Trigger shell scripts or CLI tools when data changes are detected
- **Webhook Bridges**: Wrap `curl` calls to forward data to external HTTP endpoints
- **Alerting**: Execute notification commands (e.g., send Slack messages, email) when thresholds are crossed

**Best for**: Linux environments where integrating with existing shell scripts or CLI tools is preferred.

## Configuration

### Builder Pattern (Recommended)

The builder pattern provides a fluent, type-safe API:

```rust
use drasi_reaction_shell::{ShellReaction, ShellReactionBuilder, ShellCommand, ShellExtension};
use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
use std::sync::Arc;

let reaction = ShellReactionBuilder::new("my-shell-reaction")
    .with_query("sensor-alerts")
    .with_command(
        "sensor-alerts",
        ShellCommand {
            executable: "/usr/local/bin/handle-alert.sh".to_string(),
            args: vec![],
        },
    )
    .with_env("DRASI_INSTANCE", "production")
    .with_timeout_s(30)
    .with_max_concurrent(50)
    .build()?;

drasi.add_reaction(Arc::new(reaction)).await?;
```

#### With Custom Stdin Templates

Use Handlebars templates to control exactly what is written to stdin:

```rust
use drasi_reaction_shell::{ShellReactionBuilder, ShellCommand, ShellExtension};
use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
use std::collections::HashMap;
use std::sync::Arc;

let added_template = TemplateSpec {
    template: r#"{"event":"add","id":"{{after.id}}","temp":{{after.temperature}}}"#.to_string(),
    extension: ShellExtension {
        env: HashMap::from([
            ("SENSOR_ID".to_string(), "{{after.id}}".to_string()),
            ("SENSOR_TEMP".to_string(), "{{after.temperature}}".to_string()),
        ]),
    },
};

let deleted_template = TemplateSpec {
    template: r#"{"event":"delete","id":"{{before.id}}"}"#.to_string(),
    extension: ShellExtension { env: HashMap::new() },
};

let sensor_config = QueryConfig {
    added: Some(added_template),
    updated: None,   // falls back to raw JSON
    deleted: Some(deleted_template),
};

let reaction = ShellReactionBuilder::new("sensor-handler")
    .with_query("sensor-readings")
    .with_command(
        "sensor-readings",
        ShellCommand {
            executable: "/opt/scripts/sensor.sh".to_string(),
            args: vec![],
        },
    )
    .with_route("sensor-readings", sensor_config)
    .build()?;
```

#### With a Default Template

Apply the same template to all subscribed queries when no query-specific route is defined:

```rust
use drasi_reaction_shell::{ShellReactionBuilder, ShellCommand, ShellExtension};
use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
use std::collections::HashMap;
use std::sync::Arc;

let default_template = QueryConfig {
    added: Some(TemplateSpec {
        template: "ADD {{after.id}}".to_string(),
        extension: ShellExtension { env: HashMap::new() },
    }),
    updated: Some(TemplateSpec {
        template: "UPDATE {{before.id}} -> {{after.id}}".to_string(),
        extension: ShellExtension { env: HashMap::new() },
    }),
    deleted: Some(TemplateSpec {
        template: "DELETE {{before.id}}".to_string(),
        extension: ShellExtension { env: HashMap::new() },
    }),
};

let reaction = ShellReactionBuilder::new("multi-query-handler")
    .with_queries(vec!["query-a".to_string(), "query-b".to_string()])
    .with_command("query-a", ShellCommand { executable: "/bin/process-a.sh".to_string(), args: vec![] })
    .with_command("query-b", ShellCommand { executable: "/bin/process-b.sh".to_string(), args: vec![] })
    .with_default_template(default_template)
    .build()?;
```

### Config Struct Approach

For programmatic configuration or deserialization scenarios:

```rust
use drasi_reaction_shell::{ShellReaction, ShellCommand, ShellReactionConfig};
use std::collections::HashMap;
use std::sync::Arc;

let config = ShellReactionConfig {
    commands: HashMap::from([(
        "device-alerts".to_string(),
        ShellCommand {
            executable: "/usr/local/bin/alert.sh".to_string(),
            args: vec!["--format".to_string(), "json".to_string()],
        },
    )]),
    env: HashMap::from([
        ("ENVIRONMENT".to_string(), "production".to_string()),
    ]),
    timeout_s: 30,
    max_concurrent: 20,
    ..Default::default()
};

let reaction = ShellReaction::new(
    "device-reaction",
    vec!["device-alerts".to_string()],
    config,
)?;
```

## Configuration Options

### ShellReactionConfig

| Name | Description | Type | Default |
|------|-------------|------|---------|
| `commands` | Maps each subscribed query ID to its shell command. Must have one entry per subscribed query. | `HashMap<String, ShellCommand>` | Empty |
| `env` | Global environment variables set for every command invocation. Values support Handlebars templates. | `HashMap<String, String>` | Empty |
| `routes` | Query-specific template configurations. Keys must match subscribed query IDs. Cannot have more entries than `commands`. | `HashMap<String, QueryConfig<ShellExtension>>` | Empty |
| `default_template` | Fallback template used when a query has no specific route entry. | `Option<QueryConfig<ShellExtension>>` | Raw JSON |
| `max_concurrent` | Maximum number of shell commands that can run simultaneously. | `u32` | `100` |
| `max_stdin_bytes` | Maximum byte size of stdin data. Invocations with larger payloads are rejected with a warning. | `usize` | `1048576` (1 MB) |
| `capture_limit` | Maximum bytes captured from each command's stdout and stderr. Output is truncated beyond this limit. | `usize` | `4096` (4 KB) |
| `timeout_s` | Maximum seconds a command may run. Timed-out commands have their entire process group killed. | `u64` | `60` |
| `kill_on_drop` | Whether to kill running child processes when the reaction is dropped (shutdown). | `bool` | `true` |
| `max_recent_invocations` | Number of most-recent invocation records to retain for observability. Set to `0` to disable history. | `u32` | `10` |

### ShellCommand

| Name | Description | Type | Required |
|------|-------------|------|----------|
| `executable` | Absolute path of the command to execute. Must be executable at configuration time. | `String` | Yes |
| `args` | Command-line arguments passed to the executable before stdin. | `Vec<String>` | No |

### QueryConfig\<ShellExtension\>

Defines per-operation stdin templates and environment variable injections for a specific query.

| Name | Description | Type | Required |
|------|-------------|------|----------|
| `added` | Template specification for ADD operations. | `Option<TemplateSpec<ShellExtension>>` | No |
| `updated` | Template specification for UPDATE operations. | `Option<TemplateSpec<ShellExtension>>` | No |
| `deleted` | Template specification for DELETE operations. | `Option<TemplateSpec<ShellExtension>>` | No |

### TemplateSpec\<ShellExtension\>

| Name | Description | Type |
|------|-------------|------|
| `template` | Handlebars template rendered to the command's stdin. If empty, sends raw JSON. | `String` |
| `extension.env` | Per-operation environment variables. Keys must match `^[A-Za-z_][A-Za-z0-9_]*$`. Values support Handlebars templates. | `HashMap<String, String>` |

### Builder Methods

| Method | Description |
|--------|-------------|
| `new(id)` | Create a new builder with the given reaction ID |
| `with_query(id)` | Add a single query subscription |
| `with_queries(ids)` | Set all query subscriptions |
| `with_command(query_id, command)` | Add a `ShellCommand` for a specific query |
| `with_commands(commands)` | Set all command mappings at once |
| `with_route(query_id, config)` | Add a per-query template configuration |
| `with_routes(routes)` | Set all route configurations at once |
| `with_default_template(template)` | Set the fallback template for unrouted queries |
| `with_env(key, value)` | Add a global environment variable |
| `with_envs(envs)` | Set all global environment variables |
| `with_max_concurrent(n)` | Set maximum concurrent command executions |
| `with_max_stdin_bytes(n)` | Set stdin size limit in bytes |
| `with_capture_limit(n)` | Set stdout/stderr capture limit in bytes |
| `with_timeout_s(s)` | Set per-command execution timeout in seconds |
| `with_kill_on_drop(bool)` | Set whether to kill processes on shutdown |
| `with_max_recent_invocations(n)` | Set invocation history window size |
| `with_priority_queue_capacity(n)` | Set internal priority queue capacity |
| `with_auto_start(bool)` | Enable/disable auto-start |
| `with_config(config)` | Apply a full `ShellReactionConfig` at once |
| `build()` | Build and validate the `ShellReaction`. Returns `anyhow::Result<ShellReaction>` |

## Stdin and Template Variables

When a query result change occurs, the reaction renders the matching template and writes the result to the command's stdin followed by a newline.

### Template Variables

| Variable | Description | Available Operations |
|----------|-------------|---------------------|
| `after` | The new/current state of the result row | ADD, UPDATE |
| `before` | The previous state of the result row | UPDATE, DELETE |
| `data` | Alias for `after` (UPDATE operations) | UPDATE |
| `operation` | The type of operation that triggered the reaction (`ADD`, `UPDATE`, `DELETE` and `AGGREGATION`) | All |
| `query_name` | The name of the query that triggered the reaction | All |

### Template Priority

1. Query-specific route for the matching operation (highest priority)
2. Default template for the matching operation (fallback when no query-specific route is defined)
3. Raw JSON serialization of the data (when no template is found)

### Environment Variable Rendering

Global `env` values and per-operation `extension.env` values both support Handlebars templates using the same context variables (`after`, `before`, `data`, `operation`, `query_name`). Per-operation env values are merged with (and override) global env values for that invocation.

## Execution Model

```
Query result change
        │
        ▼
 Render stdin template (or raw JSON)
        │
        ▼
 Check stdin size ≤ max_stdin_bytes
        │
        ▼
 Acquire concurrency semaphore permit (blocks if at max_concurrent)
        │
        ▼
 Spawn child process in new process group
   stdin ← rendered data
   env   ← global env + operation env
   args  ← ShellCommand.args
        │
        ▼
 Wait for exit (up to timeout_s seconds)
   on timeout → kill entire process group (SIGTERM then SIGKILL if needed)
        │
        ▼
 Capture stdout/stderr (up to capture_limit bytes each)
 Record invocation in recent history
 Release semaphore permit
```

## Observability

The reaction exposes runtime metrics as component properties:

| Property | Description |
|----------|-------------|
| `active_processes` | Number of commands currently running |
| `results_processed_success` | Total result diffs processed successfully since startup |
| `timeout_firings` | Total commands killed due to timeout |
| `non_zero_exits` | Total commands that exited with a non-zero status |
| `stdout_truncations` | Total invocations where stdout exceeded `capture_limit` |
| `stderr_truncations` | Total invocations where stderr exceeded `capture_limit` |
| `stdin_payload_rejections` | Total invocations rejected because stdin exceeded `max_stdin_bytes` |

Recent invocation records (up to `max_recent_invocations`) include:
- `start_timestamp` / `end_timestamp` (if the command has completed)
- `exit_status` (exit code, or `-1` on spawn failure)
- `stdout` / `stderr` (truncated to `capture_limit`)
- `executable` (the command that was run)

## Validation

Configuration is validated at construction time (both `new()` and `build()`):

- `max_concurrent`, `max_stdin_bytes`, `capture_limit`, and `timeout_s` must all be greater than 0.
- Every subscribed query must have exactly one entry in `commands`.
- Every entry in `routes` must correspond to a query ID that also exists in `commands`.
- Every query name specified in `commands` must correspond to a valid query subscription.
- Every executable in `commands` must exist on disk and have execute permission at creation time.
- Environment variable keys in `extension.env` must match `^[A-Za-z_][A-Za-z0-9_]*$`.
- All args must be non-empty strings.

## Process Security Hardening

The shell reaction executes external programs, so it should be treated as privileged code execution. Use these controls in production:

### Built-in hardening in the executor

Before each child process starts, the executor applies Linux `pre_exec` safeguards:

1. **Dedicated process group**: `setpgid(0, 0)` makes the child the leader of a new process group (`PGID == PID`), so timeout/cancellation can terminate the entire group (not just the immediate child).
2. **No privilege escalation**: `prctl(PR_SET_NO_NEW_PRIVS, 1, ...)` prevents the child and descendants from gaining additional privileges.
3. **Memory cap**: `setrlimit(RLIMIT_AS, 256 MiB)` caps virtual address space for the child process.

These controls are Linux-only and are applied per invocation.

## Example Script

A minimal script that reads from stdin and forwards the JSON payload:

```bash
#!/bin/bash
# Read the JSON payload from stdin
read -r payload

# Extract a field using jq (optional)
id=$(echo "$payload" | jq -r '.id')

echo "Processing event for id: $id"
# ... do real work here ...
```

Make the script executable before registering it:

```bash
chmod +x /path/to/your/script.sh
```



## Plugin Packaging

This reaction is compiled as a dynamic plugin (cdylib) that can be loaded by drasi-server at runtime.

**Key files:**
- `Cargo.toml` — includes `crate-type = ["lib", "cdylib"]`
- `src/descriptor.rs` — implements `ReactionPluginDescriptor` with kind `"shell"`, configuration DTO, and OpenAPI schema generation
- `src/lib.rs` — invokes `drasi_plugin_sdk::export_plugin!` to export the plugin entry point

**Building:**
```bash
cargo build -p drasi-reaction-shell --features dynamic-plugin
```

For this Linux-only crate, the compiled plugin artifact is the `.so` shared library placed in `target/debug/`, which can be copied to the server's `plugins/` directory.

For more details on the plugin descriptor pattern and configuration DTOs, see the [Reaction Developer Guide](../README.md#packaging-as-a-dynamic-plugin).