# AHP Getting Started

Watch AI agent sessions with Drasi continuous queries. This example connects
the [AHP source](../../../components/sources/ahp) to an
[Agent Host Protocol](https://github.com/microsoft/agent-host-protocol) host
and prints a line whenever:

- an agent session is **waiting on you** for input,
- a session **fails**,
- an agent has changed **20 or more files**.

No agent host needed: by default the example starts a small scripted AHP host
in-process (real AHP over WebSocket) that plays a short story of agent
activity.

## Run

```bash
cd examples/lib/ahp-getting-started
cargo run
```

```text
▶ Copilot asks a question
[console]   🙋 Upgrade to tokio 2 needs you: Bump the MSRV to 1.80?
▶ You answer Copilot
[console]   ✅ Upgrade to tokio 2 is no longer waiting
▶ Copilot rewrites a lot of code
[console]   📦 Upgrade to tokio 2 changed 37 files (+820 / -410)
▶ Claude's session crashes before you answer
[console]   ❌ Write API docs (Claude Code) failed
```

To watch a real AHP host instead, set its WebSocket URL (and token, if it
needs one):

```bash
AHP_URL=ws://localhost:4321 AHP_TOKEN=... cargo run
```

## How it works

The source streams the host's sessions as a graph; the bootstrap provider
loads what already exists when queries start:

```rust
let source = AhpSource::builder("agents")
    .with_url(&url)
    .with_bootstrap_provider(AhpBootstrapProvider::builder().with_url(&url).build()?)
    .build()?;
```

The graph looks like this (see the source README for all properties):

```text
(:Session)-[:USES_AGENT]->(:Agent)
(:Session)-[:HAS_CHAT]->(:Chat)
(:Session)-[:NEEDS_INPUT]->(:InputRequest)-[:IN_CHAT]->(:Chat)
(:Automation)-[:HAS_RUN]->(:AutomationRun)-[:CREATED_SESSION]->(:Session)
```

Three continuous queries pick out what matters:

```cypher
// waiting-on-me
MATCH (s:Session)-[:NEEDS_INPUT]->(r:InputRequest)
RETURN s.title AS session, r.message AS question

// failed-sessions
MATCH (s:Session)-[:USES_AGENT]->(a:Agent)
WHERE s.hasError = true
RETURN s.title AS session, a.displayName AS agent

// big-changes
MATCH (s:Session)
WHERE s.filesChanged >= 20
RETURN s.title AS session, s.filesChanged AS files,
       s.additions AS additions, s.deletions AS deletions
```

A log reaction prints each change to those results:

```rust
let reaction = LogReaction::builder("console")
    .with_queries(vec!["waiting-on-me".into(), "failed-sessions".into(), "big-changes".into()])
    .with_route(
        "waiting-on-me",
        templates(
            "🙋 {{after.session}} needs you: {{after.question}}",
            "🙋 {{after.session}} changed its question: {{after.question}}",
            "✅ {{before.session}} is no longer waiting",
        ),
    )
    // ... routes for failed-sessions and big-changes
    .build()?;
```

## Files

- `main.rs` — source, queries, and reaction setup
- `demo_host.rs` — the scripted in-process AHP host (uses `MockAhpHost`
  from `drasi-ahp-common`, feature `mock-host`)
