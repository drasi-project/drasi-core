# Watch Salesforce changes with Drasi

Edit an Account or Contact in Salesforce and watch query results change in your
terminal. This example uses the real Salesforce source, not a simulated feed.

## Try it

You need a Salesforce Developer Edition org or sandbox with API access, the
[Salesforce CLI](https://developer.salesforce.com/tools/salesforcecli), Rust,
a C/C++ toolchain, CMake and `protoc`. The first build can take several minutes.

From this directory, log in once:

```sh
sf org login web --instance-url https://YOUR-ORG.my.salesforce.com --alias drasi-dev
```

Start the example:

```sh
./quickstart.sh --org drasi-dev
```

Then use the Salesforce UI:

1. Create an Account with any name.
2. Create a Contact and choose that Account.
3. Rename either record, move the Contact to another Account, or clear its Account.

The terminal prints added, updated and deleted query results. It also prints
`STATE` when the current results change. Allow a few seconds for changes to
arrive; a complete scan runs every 15 seconds to catch anything missed.

The example only reads Salesforce. It does not create records, enable Change
Data Capture, or change permissions. There are no name or prefix filters: all
visible Accounts and Contacts are included. Delete your own test records when
finished.

## What the code does

Start with **[main.rs](main.rs)**. It contains the four query definitions and
three functions:

| Function | What to look for |
| --- | --- |
| `main` | Build the Salesforce source using your CLI login; shut down on Ctrl+C. |
| `build_runtime` | Connect the source, persistent queries, and an application reaction. |
| `watch` | Start Drasi and print the reaction's result changes. |

The data flow is:

```text
Salesforce records -> Salesforce source -> continuous queries -> printed changes
```

The default queries show Accounts, an Account count, Contacts, and Contacts joined
to their Accounts:

```cypher
MATCH (c:Contact)-[:Account]->(a:Account)
RETURN c.Id AS contactId, c.Name AS contact, a.Id AS accountId, a.Name AS account
```

The source discovers fields and relationships automatically. `Contact.AccountId`
becomes the `:Account` relationship; no second source or manual join is needed.
Clearing that field removes the joined row, but not the Contact.

The `support/` directory contains CLI/authentication plumbing, diagnostics and
offline fixtures. You do not need to read it to follow the Drasi example.

## Try your own query

```sh
./quickstart.sh --org drasi-dev \
  --query 'MATCH (a:Account) RETURN a.Name AS name, a.Industry AS industry'
```

Repeat `--query` to run several queries. Use `--schema` to see available fields
and relationships, or `--objects Account Contact Opportunity` to select more
objects. Lookup targets must also be selected for graph joins.

See the source's [schema and type mappings](../../../components/sources/salesforce/README.md#query-schema)
for the full query contract.

## Stop and resume

Press Ctrl+C and wait for `STOPPED`. Run the same command to resume saved queries
and catch up with changes made while the example was stopped.

State lives under `.state/<org-id>/graph-v2/`. Different object selections or
query definitions get separate namespaces, so experiments do not overwrite each
other. Use `--state-dir /absolute/path` to choose another root. Do not run two
copies against the same namespace or delete just the source or query half.

<details>
<summary>Diagnostics, advanced settings, and offline checks</summary>

- `./diagnose.sh --org drasi-dev` checks login, REST access and native topics
  without changing Salesforce or creating local query state.
- `./quickstart.sh --org drasi-dev --schema` prints metadata JSON without starting
  ingestion or creating local state.
- Embedded applications can use `drasi.get_source_schema("salesforce-source").await?`
  or `drasi.get_graph_schema().await?` after startup. These read the source's
  last successfully published schema without making Salesforce requests;
  they do not indicate that loading or reprojection has completed.
- To verify those APIs against your real developer org, run
  `SALESFORCE_TEST_ORG=YOUR_ALIAS cargo test live_schema_discovery -- --ignored --nocapture`
  from this example directory. This opt-in check compares both APIs with fresh
  Salesforce Describe responses, stops/restarts the source, and cleans up its
  isolated temporary spool. It does not change Salesforce records or settings,
  reuse your example state, or print tokens/record values.
- `--full-scan-seconds 60` reduces the frequency of repair scans and API usage.
- Set `SF_BIN` if the Salesforce CLI is not on `PATH`. It must support
  `org auth show-access-token`. Tokens stay in memory and are never printed.
- Native events require the objects to be enabled under **Setup > Change Data
  Capture**. Unavailable topics use periodic REST scans; authentication failures
  still fail explicitly.
- `READY` means the queries are running, not that the first scan is complete.
  If results stay empty, check record visibility and the selected lookup targets.
  Rapid edits may coalesce; this is a current-state view, not an audit log.
- Metadata refreshes automatically. Recovery errors never silently reset state;
  preserve the state directory and read the error.

Local verification needs no Salesforce account and never substitutes for a
requested live connection:

```sh
./setup.sh
./test-updates.sh
./quickstart.sh --local
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

The offline fixture uses the same `build_runtime` function as the live example.

</details>
