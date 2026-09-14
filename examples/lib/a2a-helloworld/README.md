# A2A Hello World example

Drives the official [A2A Hello World](https://github.com/a2aproject/a2a-samples/tree/main/samples/python/agents/helloworld) agent from Drasi query diffs.

```
HTTP invoices (port 9000) → overdue-invoices query → A2A reaction → Hello World (port 9999)
                                                              ↘ log reaction (console)
```

The A2A reaction is durable, so this example uses a temporary redb state store. There is no query bootstrap: nothing is sent to the agent until you insert an invoice.

## Prerequisites

1. Hello World agent listening on `http://127.0.0.1:9999/`:

   ```bash
   git clone https://github.com/a2aproject/a2a-samples.git
   cd a2a-samples/samples/python/agents/helloworld
   python -m venv .venv
   source .venv/bin/activate
   pip install -r requirements.txt
   python __main__.py
   ```

   Current `a2a-python` JSON-RPC 1.0 uses `SendMessage` / `CancelTask`, which matches this reaction. Older v0.3 agents that only accept `message/send` will return method-not-found.

2. This example crate (standalone workspace):

   ```bash
   cd examples/lib/a2a-helloworld
   cargo run
   ```

   Override the agent URL with `A2A_ENDPOINT` if needed.

## Try it

Insert, update, and delete via `change.http` or curl. Watch both this process (log reaction) and the Hello World terminal.

Hello World completes each task immediately (`TASK_STATE_COMPLETED`). After that:

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
