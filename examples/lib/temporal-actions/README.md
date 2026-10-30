# Follow an order from placed to delivered

Move an order through **placed -> paid -> shipped -> delivered** in PostgreSQL.
Drasi queries detect each new state and signal a Temporal workflow, which
completes when the order is delivered.

```text
PostgreSQL orders  ->  Drasi queries for each status  ->  Temporal order-1001
      source               continuous queries              signals
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
Workflow `order-1001` is running and waiting for the order to be placed.
The database has no order yet.

```sh
./demo.sh placed
```

```text
PostgreSQL: order 1001 placed
Drasi:     order entered the placed query
Temporal:  order-1001 processed placed
```

Move the same order through the remaining states:

```sh
./demo.sh paid
./demo.sh shipped
./demo.sh delivered
```

The final command prints:

```text
PostgreSQL: order 1001 changed from shipped to delivered
Drasi:     order entered the delivered query
Temporal:  order-1001 processed delivered
Temporal:  workflow completed
```

`placed` inserts the order. The other commands update its `status` column.
Each transition adds a result to the next query and removes it from the
previous query. Only additions send signals; removals have no action configured.

Each signal confirmation comes from the workflow's actual state, not just
the database commit. If delivery cannot be confirmed, the command reports an
error instead of printing success. Repeating the current state confirms its
signal without changing the row. Skipping a step or moving backward fails
before changing the database.

## See what happened

Open **[Temporal UI](http://localhost:18080)** and select `order-1001`.
Its history shows the four signals and the completed workflow result.
Set `TEMPORAL_UI_PORT` if port 18080 is already in use.

## Read the setup

`./demo.sh up` runs this setup for you. The [source table](orders/init.sql)
is PostgreSQL `orders`, with `id` and `status` columns and logical replication
enabled.

### Temporal workflow

The workflow in [orders/workflow.py](orders/workflow.py) waits for each signal
before advancing. Waiting consumes no polling loop or database connection in
the workflow:

```python
@workflow.defn
class OrderLifecycle:
	def __init__(self):
		self.status = "awaiting_placement"
		self.events = []

	@workflow.run
	async def run(self):
		for stage in ("placed", "paid", "shipped", "delivered"):
			await workflow.wait_condition(
				lambda: any(event["signal"] == stage for event in self.events)
			)
			self.status = stage
		return self.progress()

	@workflow.signal
	def placed(self, diff: dict):
		self.events.append({"signal": "placed", "diff": diff})

	@workflow.signal
	def paid(self, diff: dict):
		self.events.append({"signal": "paid", "diff": diff})

	@workflow.signal
	def shipped(self, diff: dict):
		self.events.append({"signal": "shipped", "diff": diff})

	@workflow.signal
	def delivered(self, diff: dict):
		self.events.append({"signal": "delivered", "diff": diff})

	@workflow.query
	def progress(self) -> dict:
		return {"status": self.status, "events": self.events}
```

The file imports `workflow` from the Temporal Python SDK. Its Worker registers
`OrderLifecycle` on the `orders` task queue and starts it with workflow ID
`order-1001` only while the database has no order. Once an order exists, startup
requires the existing execution rather than creating a replacement. A completed
execution can be resumed for inspection only while Temporal retains its history.
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

[orders/reaction.json](orders/reaction.json) maps additions from each query
to the corresponding signal:

```json
{
	"endpoint": "http://temporal:7233",
	"outputTemplates": {
		"routes": {
			"placed": {
				"added": {
					"action": "signal",
					"workflowId": "order-1001",
					"signalName": "placed"
				}
			},
			"paid": {
				"added": {
					"action": "signal",
					"workflowId": "order-1001",
					"signalName": "paid"
				}
			},
			"shipped": {
				"added": {
					"action": "signal",
					"workflowId": "order-1001",
					"signalName": "shipped"
				}
			},
			"delivered": {
				"added": {
					"action": "signal",
					"workflowId": "order-1001",
					"signalName": "delivered"
				}
			}
		}
	}
}
```

`temporal` is the Compose service name. Each route matches a query ID, and
each `signalName` matches a workflow handler. With no payload template,
the reaction sends the standard diff envelope as the handler's `diff` argument,
including the operation, sequence ID, and applicable `before` and `after` rows.

[main.rs](main.rs) loads that JSON into `raw` and creates reaction `actions`,
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

The reaction only sends signals. The Worker starts the workflow, and the
workflow decides when to complete. The order story and workflow ID exist only
in this example, not in the reusable reaction.

## Stop

```sh
./demo.sh down
```

Docker volumes are preserved. `./demo.sh up` resumes with the same order and
workflow while Temporal retains the execution. The development server's default
namespace retains **closed workflow history for 24 hours**; that retention still
applies when its database is stored in a volume. Running workflows do not expire
under this retention policy.

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

Keep those environment variables set when running the stage commands and `down`.
This leaves the original project's data untouched. To delete a run's saved data
after stopping it:

```sh
docker compose -f orders/compose.yaml down --volumes
```

That removes all three of that project's data volumes together. Do not delete
only one of them and keep unrelated checkpoints from another run.

This story uses a new Compose project, `drasi-temporal-orders`, so it does not
reuse the previous inventory demo's data. If that demo still occupies port
18080, stop its containers first or choose another `TEMPORAL_UI_PORT`.

## Go deeper

- [Action configuration reference](../../../components/reactions/temporal/README.md)
- Run `./verify-demo.sh` to repeat the walkthrough, check workflow state, and
  exercise repeated startup and restart. It also runs the Python command-confirmation
  tests and removes the test workflow's history to verify that an existing order
  is never silently restarted after history loss. This is a manual check, not a CI
  job. It uses its own temporary Compose project and removes only that project's data.

Like the other `examples/lib` demos, this is a standalone Cargo workspace.
Its committed `Cargo.lock` pins the Docker build's dependencies independently
of the root workspace; workspace CI does not build this example.
