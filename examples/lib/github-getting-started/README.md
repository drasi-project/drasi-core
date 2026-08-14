# GitHub Source Getting Started (DrasiLib)

This example runs the authorized GitHub source plugin with a **local client harness**:

- Local mock GitHub GraphQL server (`/graphql`)
- Local control API to mutate authoritative issue state (`/control/issue`)
- Signed webhook POSTs to the source listener
- Drasi `LogReaction` output showing INSERT / UPDATE / DELETE detection

No Docker dependency is required.

## Prerequisites

- Rust toolchain
- `curl`
- `python3`

## Quick Start

Terminal 1:

```bash
cd examples/lib/github-getting-started
./quickstart.sh
```

Terminal 2:

```bash
cd examples/lib/github-getting-started
./test-updates.sh
```

## How to Verify It's Working

When `test-updates.sh` runs, the terminal running `cargo run` should print:

- `➕ ISSUE INSERTED ...`
- `🔄 ISSUE UPDATED ...`
- `➖ ISSUE DELETED ...`

This confirms end-to-end change detection:

webhook admission → WAL → hydrator authoritative GraphQL fetch → SourceChange dispatch → query result diff → reaction output.

## Helper Scripts

- `setup.sh` validates local prerequisites, the data directory, and waits up to 60s for required ports to be free.

- `quickstart.sh` runs setup, then starts the example.

- `diagnose.sh` checks local mock GraphQL health, source webhook health, and control state.

- `test-updates.sh` simulates CREATE / UPDATE / DELETE by mutating control state and posting signed webhooks.

## Troubleshooting

- `setup.sh` fails with port-in-use:
  - Stop conflicting process or override `GITHUB_EXAMPLE_GRAPHQL_ADDR` / `GITHUB_EXAMPLE_WEBHOOK_PORT`.
- `test-updates.sh` returns `401`:
  - Ensure `GITHUB_EXAMPLE_WEBHOOK_SECRET` matches both shells.
- No reaction output:
  - Check `./diagnose.sh` and ensure `/health` is healthy.

## Environment Overrides

Optional environment variables:

- `GITHUB_EXAMPLE_GRAPHQL_ADDR` (default: `127.0.0.1:19080`)
- `GITHUB_EXAMPLE_WEBHOOK_HOST` (default: `127.0.0.1`)
- `GITHUB_EXAMPLE_WEBHOOK_PORT` (default: `19081`)
- `GITHUB_EXAMPLE_WEBHOOK_PATH` (default: `/webhook`)
- `GITHUB_EXAMPLE_WEBHOOK_SECRET` (default: `example-secret`)
- `GITHUB_EXAMPLE_DATA_DIR` (default: `.data`)
