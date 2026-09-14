# A2A Hello World example

HTTP invoices → Cypher query → A2A reaction against a local Hello World agent.

```
HTTP invoices (port 9000) → overdue-invoices query → A2A reaction → Hello World (port 9999)
                                                              ↘ log reaction (console)
```

`./run.sh` starts a bundled stdlib Python agent on port 9999, then this crate. No clone, venv, pip, or `a2a-sdk`.

The A2A reaction is durable, so the example uses a temporary redb store. There is no query bootstrap: nothing is sent until you insert an invoice.

## Run

```bash
cd examples/lib/a2a-helloworld
./run.sh
```

Override the agent URL with `A2A_ENDPOINT` if needed.

## Try it

Insert, update, and delete via `change.http` or curl. Watch this process (log reaction) and the agent logs.

The bundled agent completes each task immediately (`completed`). After that:

- a later **update** starts a new `SendMessage` (replace policy)
- a later **delete** is dropped — there is no live task to `CancelTask`

## Query

```cypher
MATCH (inv:Invoice)
RETURN inv.invoiceId AS invoiceId,
       inv.customer AS customer,
       inv.amount AS amount
```

Result key: `invoiceId`. Instruction sent to the agent:

`Say hello. Overdue invoice {{after.invoiceId}} for {{after.customer}} (amount {{after.amount}}).`

## Official A2A sample (optional)

The bundled `agent.py` speaks the same JSON-RPC 1.0 `SendMessage` / `CancelTask` surface as current `a2a-python`. To use [a2a-samples helloworld](https://github.com/a2aproject/a2a-samples/tree/main/samples/python/agents/helloworld) instead:

```bash
python3 -m venv .venv
source .venv/bin/activate
pip install --index-url https://pypi.org/simple -r requirements.txt
python3 __main__.py
```

Then `A2A_ENDPOINT=http://127.0.0.1:9999/ cargo run` from this directory (do not start `./run.sh`, which binds 9999 itself).
