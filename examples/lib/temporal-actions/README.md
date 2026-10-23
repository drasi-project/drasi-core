# Low stock? Tell a Temporal workflow.

Change a stock level in PostgreSQL. Drasi detects when it enters, changes
within, or leaves a low-stock query and signals a running Temporal workflow.

```text
PostgreSQL inventory  ->  Drasi: stock < 5  ->  Temporal inventory-monitor
      source              continuous query        signals via reaction
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

Wait for `Ready`. The demo starts PostgreSQL, Drasi, Temporal, and its Worker,
then creates one inventory row: `coffee-beans`, stock **20**.

```sh
./demo.sh stock 4
```

```text
PostgreSQL: stock changed from 20 to 4
Drasi:     product entered the low-stock query
Temporal:  inventory-monitor processed low_stock
```

Now change it twice more:

```sh
./demo.sh stock 2
./demo.sh stock 20
```

| Stock change | Query result change | Signal |
| --- | --- | --- |
| 20 to 4 | Added: now low | `low_stock` |
| 4 to 2 | Updated: still low | `stock_changed` |
| 2 to 20 | Deleted: no longer low | `stock_restored` |

**All three commands update the same database row.** A deleted query result
means the row stopped matching, not that the database row was deleted.

Each signal confirmation comes from the workflow's actual state, not just
the database commit. If delivery cannot be confirmed, the command reports an
error instead of printing success. Setting the existing value makes no change;
moving between two values of 5 or higher requires no signal.

## See what happened

Open **[Temporal UI](http://localhost:18080)** and select `inventory-monitor`.
Its history shows the three signals. The workflow stays running throughout.
Set `TEMPORAL_UI_PORT` if port 18080 is already in use.

## Read the setup

`./demo.sh up` runs this setup for you. The [source table](inventory/init.sql)
is PostgreSQL `inventory`, with logical replication enabled.

### Temporal workflow

The workflow in [inventory/workflow.py](inventory/workflow.py) stays running
and records each signal's payload:

```python
@workflow.defn
class InventoryMonitor:
	def __init__(self):
		self.events = []

	@workflow.run
	async def run(self):
		await workflow.wait_condition(lambda: False)

	@workflow.signal
	def low_stock(self, diff: dict):
		self.events.append({"signal": "low_stock", "diff": diff})

	@workflow.signal
	def stock_changed(self, diff: dict):
		self.events.append({"signal": "stock_changed", "diff": diff})

	@workflow.signal
	def stock_restored(self, diff: dict):
		self.events.append({"signal": "stock_restored", "diff": diff})

	@workflow.query
	def events_received(self) -> list:
		return self.events
```

The file imports `workflow` from the Temporal Python SDK. Its Worker registers
`InventoryMonitor` on the `inventory` task queue and starts it with workflow ID
`inventory-monitor`. On restart, it reuses the running workflow. The demo
commands query `events_received` to confirm that each signal was processed.

### Continuous query

[inventory/query.cypher](inventory/query.cypher) selects low-stock products:

```cypher
MATCH (i:inventory)
WHERE i.stock < 5
RETURN i.product AS product, i.stock AS stock
```

The PostgreSQL source maps rows to `inventory` nodes. In [main.rs](main.rs),
the query is named `changes` and reads from source `input`:

```rust
let query = Query::cypher("changes")
	.query(query_text)
	.from_source("input")
	.enable_bootstrap(false)
	.auto_start(true)
	.build();
```

`query_text` contains the Cypher file above. Bootstrap is disabled because the
demo inserts its initial row after Drasi is ready.

### Reaction mappings

[inventory/reaction.json](inventory/reaction.json) maps each kind of query
diff to a signal on the same workflow:

```json
{
	"endpoint": "http://temporal:7233",
	"outputTemplates": {
		"routes": {
			"changes": {
				"added": {
					"action": "signal",
					"workflowId": "inventory-monitor",
					"signalName": "low_stock"
				},
				"updated": {
					"action": "signal",
					"workflowId": "inventory-monitor",
					"signalName": "stock_changed"
				},
				"deleted": {
					"action": "signal",
					"workflowId": "inventory-monitor",
					"signalName": "stock_restored"
				}
			}
		}
	}
}
```

`temporal` is the Compose service name. The `changes` route matches the query
ID, and each `signalName` matches a workflow handler. With no payload template,
the reaction sends the standard diff envelope as the handler's `diff` argument,
including the operation, sequence ID, and applicable `before` and `after` rows.

[main.rs](main.rs) loads that JSON into `raw` and creates reaction `actions`,
subscribed to query `changes`:

```rust
let reaction = TemporalReactionDescriptor
	.create_reaction("actions", vec!["changes".into()], &raw, true)
	.await?;
```

The reaction never starts or cancels the monitor in this demo. The inventory
story and workflow ID exist only in the example, not in the reusable reaction.

## Stop

```sh
./demo.sh down
```

Data is preserved. `./demo.sh up` resumes with the same stock level and
workflow history. It does not reset them to 20.

This is a local development demo with fixed demo credentials. Only Temporal's
UI is exposed, on loopback. The database and Drasi status endpoint are internal
to the Compose network.

For a fresh independent run, set `COMPOSE_PROJECT_NAME` to a new name before
running the commands. To delete a run's saved data after stopping it:

```sh
docker compose -f inventory/compose.yaml down --volumes
```

That removes all three of that project's data volumes together. Do not delete
only one of them and keep unrelated checkpoints from another run.

## Go deeper

- [Action configuration reference](../../../components/reactions/temporal/README.md)
- Run `./verify-demo.sh` to repeat the walkthrough, check workflow state, and
  exercise repeated startup and restart. It uses its own temporary Compose
  project and removes only that project's data.
