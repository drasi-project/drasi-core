#!/usr/bin/env bash
# Copyright 2026 The Drasi Authors.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

example_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
compose=(docker compose --file "$example_dir/orders/compose.yaml")
case "${1:-}" in
    up)
        [[ $# -eq 1 ]] || { echo "Usage: ./demo.sh up" >&2; exit 2; }
        echo "Starting PostgreSQL, Drasi, and Temporal. The first build takes several minutes."
        if ! "${compose[@]}" up --build --detach --wait --wait-timeout 120; then
            "${compose[@]}" logs --no-color --tail=20 worker drasi >&2
            exit 1
        fi
        "${compose[@]}" exec -T worker python demo.py ready
        ui_address="$("${compose[@]}" port temporal 8233)"
        echo "Temporal UI: http://$ui_address"
        ;;
    placed|paid|shipped|delivered)
        [[ $# -eq 1 ]] || { echo "Usage: ./demo.sh $1" >&2; exit 2; }
        "${compose[@]}" exec -T worker python demo.py "$1"
        ;;
    down)
        [[ $# -eq 1 ]] || { echo "Usage: ./demo.sh down" >&2; exit 2; }
        "${compose[@]}" down
        echo "Stopped. Data is preserved; resuming requires retained Temporal workflow history."
        ;;
    *)
        echo "Usage: ./demo.sh up | placed | paid | shipped | delivered | down" >&2
        exit 2
        ;;
esac
