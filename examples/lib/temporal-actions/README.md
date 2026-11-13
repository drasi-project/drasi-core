# Follow an order from placed to delivered

Move an order through **placed -> paid -> shipped -> delivered** in PostgreSQL.
Drasi queries detect each new state. The first starts a Temporal workflow;
the remaining states signal it until the order is delivered.

```text
PostgreSQL orders  ->  Drasi queries for each status  ->  Temporal order-1001
      source               continuous queries            start + signals
```

## Try it

You need **Docker with Compose**. No local Rust or Python installation.
The first run builds Drasi and takes several minutes; later runs reuse the images.
If your network requires a Python package mirror, set `PIP_INDEX_URL` before
starting. The default is the public PyPI index.

```sh
cd examples/lib/temporal-actions
./demo.sh up
```

Wait for `Ready`. The demo starts PostgreSQL, Drasi, Temporal, and its Worker.
The database has no order yet, and workflow `order-1001` has not started.

```sh
./demo.sh placed
```

```text
SQL:        INSERT INTO orders (id, status) VALUES ('1001', 'placed');
PostgreSQL: order 1001 placed
Drasi:     order entered the placed query
Temporal:  order-1001 started from placed
```

Move the same order through the remaining states:

```sh
./demo.sh paid
./demo.sh shipped
./demo.sh delivered
```

The final command prints:

```text
SQL:        UPDATE orders SET status = 'delivered' WHERE id = '1001';
PostgreSQL: order 1001 changed from shipped to delivered
Drasi:     order entered the delivered query
Temporal:  order-1001 processed delivered
Temporal:  workflow completed
```

`placed` inserts the order. The other commands update its `status` column.
Each command prints the executed mutation with its parameter values inlined
for readability; the database operation itself uses bound parameters.
Each transition adds a result to the next query and removes it from the
previous query. The placed addition starts the workflow; later additions send
signals. Removals have no action configured.

Each action confirmation comes from the workflow's actual state, not just
the database commit. If delivery cannot be confirmed, the command reports an
error instead of printing success. Repeating the current state confirms its
signal without changing the row. Skipping a step or moving backward fails
before changing the database.

## See what happened

Open **[Temporal UI](http://localhost:18080)** and select `order-1001`.
Its history shows the start, the three signals, and the completed workflow result.
Set `TEMPORAL_UI_PORT` if port 18080 is already in use.

## Read the setup

`./demo.sh up` runs this setup for you. The [source table](orders/init.sql)
is PostgreSQL `orders`, with `id` and `status` columns and logical replication
enabled.

### Temporal workflow

The workflow in [orders/workflow.py](orders/workflow.py) receives the placed
diff as its start input, then waits for each remaining signal before advancing.
Waiting consumes no polling loop or database connection in the workflow:

```python
@workflow.defn
class OrderLifecycle:
	def __init__(self):
		self.status = "awaiting_placement"
		self.events = []

	@workflow.run
	async def run(self, placed: dict):
		self.events.insert(0, {"stage": "placed", "diff": placed})
		self.status = "placed"
		for stage in ("paid", "shipped", "delivered"):
			await workflow.wait_condition(
				lambda: any(event["stage"] == stage for event in self.events)
			)
			self.status = stage
		return self.progress()

	@workflow.signal
	def paid(self, diff: dict):
		self.events.append({"stage": "paid", "diff": diff})

	@workflow.signal
	def shipped(self, diff: dict):
		self.events.append({"stage": "shipped", "diff": diff})

	@workflow.signal
	def delivered(self, diff: dict):
		self.events.append({"stage": "delivered", "diff": diff})

	@workflow.query
	def progress(self) -> dict:
		return {"status": self.status, "events": self.events}
```

The file imports `workflow` from the Temporal Python SDK. Its Worker registers
`OrderLifecycle` on the `orders` task queue; it does not start workflows.
The reaction starts `order-1001` when the order is placed. A completed execution
can be resumed for inspection only while Temporal retains its history.
The demo commands query `progress` while it runs and read its result after it
completes. The workflow buffers received signals so that its stages advance
in order even if signals arrive in a different order.

### Continuous queries

Each file in [orders/queries/](orders/queries/) selects one status.
The query ID is the file name without `.cypher`.

`placed.cypher`:

```cypher
MATCH (o:orders)
WHERE o.status = 'placed'
RETURN o.id AS orderId, o.status AS status
```

`paid.cypher`:

```cypher
MATCH (o:orders)
WHERE o.status = 'paid'
RETURN o.id AS orderId, o.status AS status
```

`shipped.cypher`:

```cypher
MATCH (o:orders)
WHERE o.status = 'shipped'
RETURN o.id AS orderId, o.status AS status
```

`delivered.cypher`:

```cypher
MATCH (o:orders)
WHERE o.status = 'delivered'
RETURN o.id AS orderId, o.status AS status
```

The PostgreSQL source maps rows to `orders` nodes. [main.rs](main.rs) registers
all four queries against source `input`:

```rust
for id in QUERY_IDS {
	let query_text = std::fs::read_to_string(query_dir.join(format!("{id}.cypher")))?;
	let query = Query::cypher(id)
		.query(query_text)
		.from_source("input")
		.enable_bootstrap(false)
		.auto_start(true)
		.build();
	builder = builder.with_query(query);
}
```

`QUERY_IDS` is `["placed", "paid", "shipped", "delivered"]`. Bootstrap is
disabled because the demo places the order after Drasi is ready.

### Reaction mappings

[orders/reaction.yaml](orders/reaction.yaml) maps the placed addition to a
workflow start and the other additions to signals:

```yaml
endpoint: http://temporal:7233
outputTemplates:
  routes:
    placed:
      added:
        action: start
        workflowId: "order-{{after.orderId}}"
        workflowType: OrderLifecycle
        taskQueue: orders
    paid:
      added:
        action: signal
        workflowId: "order-{{after.orderId}}"
        signalName: paid
    shipped:
      added:
        action: signal
        workflowId: "order-{{after.orderId}}"
        signalName: shipped
    delivered:
      added:
        action: signal
        workflowId: "order-{{after.orderId}}"
        signalName: delivered
```

`temporal` is the Compose service name. Each route matches a query ID, and
each `signalName` matches a workflow handler. The `workflowId` template reads
`orderId` from the added query result: order `1001` routes to `order-1001`.
With no payload template, the reaction sends the standard diff envelope as
the workflow's start input or signal handler's `diff` argument, including the
operation, sequence ID, and applicable `before` and `after` rows.

[main.rs](main.rs) loads that YAML into `raw` and creates reaction `actions`,
subscribed to all four queries:

```rust
let reaction = TemporalReactionDescriptor
	.create_reaction(
		"actions",
		QUERY_IDS.iter().map(|id| (*id).into()).collect(),
		&raw,
		true,
	)
	.await?;
```

The reaction starts the workflow and sends signals; the workflow decides when
to complete. The order story and workflow ID exist only in this example, not
in the reusable reaction.

## Start again

```sh
./demo.sh down
./demo.sh up
```

`down` stops the services and **deletes the current Compose project's data**,
including the PostgreSQL orders, Temporal workflow history, and Drasi
checkpoints. The next `up` starts a fresh order. Images are kept for reuse.

To stop without deleting data, use `./demo.sh stop`. A later `./demo.sh up`
resumes the same order and workflow while Temporal retains the execution. The
development server's default namespace retains **closed workflow history for
24 hours**; that retention still applies when its database is stored in a
volume. Running workflows do not expire under this retention policy.

If upgrading from the earlier demo that started the workflow before placement,
run `./demo.sh down` before starting again, or use a fresh Compose project.
The old workflow history and any pending reaction actions use the earlier
signal-based definition and cannot safely resume with this version.

After history expires, PostgreSQL and Drasi may still contain the delivered order
and its acknowledged checkpoints. The demo cannot reconstruct the workflow from
those checkpoints. Startup reports missing history instead of silently creating
an empty replacement. `REJECT_DUPLICATE` protects workflow IDs only while their
closed executions remain within Temporal's
[retention period](https://docs.temporal.io/workflow-execution/workflowid-runid#what-is-a-workflow-id-reuse-policy).

This is a local development demo with fixed demo credentials. Only Temporal's
UI is exposed, on loopback. The database and Drasi status endpoint are internal
to the Compose network.

For a fresh independent run, set `COMPOSE_PROJECT_NAME` to a new name before
running the commands. Use another UI port if the previous project is still running:

```sh
export COMPOSE_PROJECT_NAME=drasi-temporal-orders-fresh
export TEMPORAL_UI_PORT=18081
./demo.sh up
```

Keep those environment variables set when running the stage commands and `down`
or `stop`. `down` removes only the selected project's data volumes together,
leaving the original project's data untouched. Do not delete only one volume
and keep unrelated checkpoints from another run.

This story uses a new Compose project, `drasi-temporal-orders`, so it does not
reuse the previous inventory demo's data. If that demo still occupies port
18080, stop its containers first or choose another `TEMPORAL_UI_PORT`.

## Go deeper

- [Action configuration reference](../../../components/reactions/temporal/README.md)
- Run `./verify-demo.sh` to repeat the walkthrough, check workflow state, and
  exercise restart, data preservation, and a fresh run after `down`. It also runs
  the Python command-confirmation tests, checks routing for a second order ID,
  and removes the test workflow's history to verify that an existing order is
  never silently restarted after history loss. This is a manual check, not a CI
  job. It uses its own temporary Compose project and removes only that project's data.

Like the other `examples/lib` demos, this is a standalone Cargo workspace.
Its committed `Cargo.lock` pins the Docker build's dependencies independently
of the root workspace. CI checks that the lockfile is current, but does not
build this example. After changing its dependencies or their local crate
versions, update the lockfile from the repository root with:

```sh
cargo metadata --format-version 1 --manifest-path examples/lib/temporal-actions/Cargo.toml > /dev/null
```
