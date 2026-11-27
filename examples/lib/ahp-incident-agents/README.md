# AHP Incident Agents

Every **critical, open incident** gets its own agent session on an
[Agent Host Protocol (AHP)](https://github.com/microsoft/agent-host-protocol) host,
kept up to date as the incident changes — no polling, no glue code.

```
incidents ──▶ critical-incidents (Cypher) ──▶ AHP reaction ──▶ agent sessions
```

| Incident change | What the agent host sees |
|---|---|
| Becomes critical (row added) | New session titled `INC-1: …`, first turn with the incident details |
| Severity changes (row updated) | Follow-up message queued on the same chat |
| Resolved (row removed) | Active turn cancelled and the session archived |

## Run it

```bash
cd examples/lib/ahp-incident-agents
cargo run
```

By default the example starts a built-in **mock agent host** (from the reaction's
`mock-host` feature), so it runs with no external services. To use a real AHP host:

```bash
AHP_ENDPOINT=ws://127.0.0.1:8080 cargo run
```

Sample output:

```
> INC-1 opened: checkout API 500s (sev 2)
  [host]  new agent session "INC-1: Checkout API returns 500s"
  [host]  turn started: Incident INC-1 (sev 2) on checkout: Checkout API returns 500s. Find the root cause and propose a fix.

> INC-2 opened: slow dashboard (sev 4, not critical)
  [agent] Looked into it: Incident INC-1 (sev 2) on checkout: ...

> INC-1 escalated to sev 1
  [host]  follow-up queued: Update on INC-1: severity changed from 2 to 1.
  [host]  turn started: Update on INC-1: severity changed from 2 to 1.

> INC-2 escalated to sev 2
  [host]  new agent session "INC-2: Dashboard is slow"
  ...

> INC-1 resolved
  [host]  session archived: ahp-session:/drasi-...
```

## The query

Only open incidents with severity 1–2 are in the result set, so a row is added
when an incident becomes critical and removed when it is resolved or downgraded.

```rust
let query = Query::cypher("critical-incidents")
    .query(
        r#"
        MATCH (i:Incident)
        WHERE i.status = 'open' AND i.severity <= 2
        RETURN i.id AS id, i.title AS title, i.service AS service, i.severity AS severity
        "#,
    )
    .from_source("incidents")
    .auto_start(true)
    .enable_bootstrap(false)
    .build();
```

## The reaction

`resultKeyFields` ties each incident to one session. Templates are Handlebars
over the change (`after`, `before`, `query_id`, ...).

```rust
let agents = AhpReaction::builder("incident-agents")
    .with_query("critical-incidents")
    .with_endpoint(&endpoint)
    .with_result_key_fields(["id"])
    .with_title_template("{{after.id}}: {{after.title}}")
    .with_message_template(
        "Incident {{after.id}} (sev {{after.severity}}) on {{after.service}}: \
         {{after.title}}. Find the root cause and propose a fix.",
    )
    .with_follow_up_template(
        "Update on {{after.id}}: severity changed from {{before.severity}} to {{after.severity}}.",
    )
    .with_on_delete(OnDelete::Archive)
    .build()?;
```

## The workflow

The app pushes incident changes into an application source; Drasi does the rest.

```rust
let drasi = DrasiLib::builder()
    .with_source(source)
    .with_query(query)
    .with_reaction(agents)
    .build()
    .await?;
drasi.start().await?;

let props = incident("INC-1", "Checkout API returns 500s", "checkout", 2, "open");
incidents.send_node_insert("INC-1", vec!["Incident"], props.build()).await?;   // new session

let props = incident("INC-1", "Checkout API returns 500s", "checkout", 1, "open");
incidents.send_node_update("INC-1", vec!["Incident"], props.build()).await?;   // follow-up

let props = incident("INC-1", "Checkout API returns 500s", "checkout", 1, "resolved");
incidents.send_node_update("INC-1", vec!["Incident"], props.build()).await?;   // archived
```

See the [AHP reaction README](../../../components/reactions/ahp/README.md) for
automation mode (`runAutomation`), authentication, and all configuration options.
