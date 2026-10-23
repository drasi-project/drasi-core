#!/usr/bin/env bash
# Copyright 2026 The Drasi Authors.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

example_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
compose=(docker compose --file "$example_dir/inventory/compose.yaml")
case "${1:-}" in
    up)
        [[ $# -eq 1 ]] || { echo "Usage: ./demo.sh up" >&2; exit 2; }
        echo "Starting PostgreSQL, Drasi, and Temporal. The first build takes several minutes."
        "${compose[@]}" up --build --detach --wait --wait-timeout 120
        "${compose[@]}" exec -T worker python demo.py seed
        ui_address="$("${compose[@]}" port temporal 8233)"
        echo "Temporal UI: http://$ui_address"
        echo "Try: ./demo.sh stock 4"
        ;;
    stock)
        [[ $# -eq 2 && "$2" =~ ^[0-9]+$ && ${#2} -le 9 ]] || {
            echo "Usage: ./demo.sh stock NONNEGATIVE_INTEGER (up to 9 digits)" >&2; exit 2;
        }
        "${compose[@]}" exec -T worker python demo.py stock "$2"
        ;;
    down)
        [[ $# -eq 1 ]] || { echo "Usage: ./demo.sh down" >&2; exit 2; }
        "${compose[@]}" down
        echo "Stopped. Data is preserved; ./demo.sh up resumes the demo."
        ;;
    *)
        echo "Usage: ./demo.sh up | stock NUMBER | down" >&2
        exit 2
        ;;
esac
