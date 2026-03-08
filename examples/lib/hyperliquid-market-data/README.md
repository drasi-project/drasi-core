# Hyperliquid Market Data Example

This example streams Hyperliquid market data into DrasiLib and prints query results to the console.

## Overview

The example demonstrates:
- Hyperliquid REST bootstrap (Coin + MidPrice data)
- WebSocket streaming for trades, order books, mid prices, liquidations
- LogReaction output for continuous query results

## Quick Start

```bash
./quickstart.sh
```

This script runs `setup.sh` to verify connectivity, then starts the example.

## How to Verify It's Working

Once running, you should see console output from the LogReaction showing:
- BTC mid price updates (`btc-price-alerts` query)
- Large trades (`large-trades` query)

Use `test-updates.sh` to verify that Hyperliquid streams are active:

```bash
./test-updates.sh
```

## Helper Scripts

| Script | Purpose |
| --- | --- |
| `setup.sh` | Verify REST/WebSocket connectivity (60s timeout) |
| `quickstart.sh` | One-command setup + run |
| `diagnose.sh` | Troubleshooting connectivity |
| `test-updates.sh` | Confirm trade updates are flowing |

## Running Manually

```bash
cargo run
```

Enable debug logs:

```bash
RUST_LOG=debug cargo run
```

## Troubleshooting

- **REST API unreachable**: check `https://api.hyperliquid.xyz/info` connectivity
- **WebSocket errors**: install `wscat` (`npm i -g wscat`) and retry
- **No output**: ensure `enable_trades`/`enable_mid_prices` are enabled in `main.rs`
