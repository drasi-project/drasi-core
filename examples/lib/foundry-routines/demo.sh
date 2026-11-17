#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
case "${1:-}" in
  --stop)
    if curl --fail --silent http://127.0.0.1:8780/api/health >/dev/null; then
      curl --fail --silent -X POST http://127.0.0.1:8780/api/shutdown
    else
      printf '%s\n' 'Backend unavailable; stopping only the database fixtures.'
    fi
    docker compose down
    exit ;;
  --offline) mode=(--offline) ;;
  "") mode=() ;;
  *) printf '%s\n' 'Usage: bash demo.sh [--offline|--stop]' >&2; exit 2 ;;
esac
if [[ ! -f .env ]]; then
  cp .env.example .env
  printf '%s\n' 'Created .env. Set FOUNDRY_PROJECT_ENDPOINT there for live agent calls.'
fi
docker compose up -d --wait
(cd ui && npm ci && npm run build)
cargo run --locked -- --fixtures "${mode[@]}"
