# A2A Hello World example

Drives the official [A2A Hello World](https://github.com/a2aproject/a2a-samples/tree/main/samples/python/agents/helloworld) agent from Drasi query diffs.

```
HTTP invoices (port 9000) → overdue-invoices query → A2A reaction → Hello World (port 9999)
                                                              ↘ log reaction (console)
```

The A2A reaction is durable, so this example uses a temporary redb state store. There is no query bootstrap: nothing is sent to the agent until you insert an invoice.

Hello World requires **Python 3.10+**. macOS `/usr/bin/python3` is often 3.9; `a2a-sdk` then fails with `from versions: none`. Use a 3.10+ interpreter (`python3.12`, Homebrew `python3`, etc.).

## 1. Start Hello World

```bash
git clone https://github.com/a2aproject/a2a-samples.git
cd a2a-samples/samples/python/agents/helloworld
python3.12 -m venv .venv
source .venv/bin/activate
python -m pip install -r requirements.txt
python __main__.py
```

It listens on `http://127.0.0.1:9999/`. Current `a2a-python` JSON-RPC 1.0 uses `SendMessage` / `CancelTask`, which matches this reaction.

## 2. Start this example

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
