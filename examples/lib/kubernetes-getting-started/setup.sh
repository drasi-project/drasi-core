#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
K3S_CONTAINER="drasi-k3s-example"

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required command: $1"
    exit 1
  fi
}

require_cmd docker
require_cmd kubectl

if kubectl cluster-info >/dev/null 2>&1; then
  echo "Using existing Kubernetes cluster from current kubeconfig."
  exit 0
fi

echo "No reachable Kubernetes cluster found. Starting local k3s container..."
if docker ps -a --format '{{.Names}}' | grep -Fxq "$K3S_CONTAINER"; then
  docker rm -f "$K3S_CONTAINER" >/dev/null
fi

docker run -d \
  --name "$K3S_CONTAINER" \
  --privileged \
  -p 6443:6443 \
  rancher/k3s:v1.32.2-k3s1 \
  server --disable=traefik >/dev/null

echo "Waiting for k3s to generate kubeconfig..."
for i in $(seq 1 30); do
  if docker exec "$K3S_CONTAINER" test -f /etc/rancher/k3s/k3s.yaml 2>/dev/null; then
    break
  fi
  if [ "$i" -eq 30 ]; then
    echo "✗ k3s did not generate kubeconfig within 60s"
    docker logs "$K3S_CONTAINER" --tail 20
    exit 1
  fi
  [ $((i % 5)) -eq 0 ] && echo "  Still waiting... ($i/30)"
  sleep 2
done

mkdir -p "$HOME/.kube"
docker cp "$K3S_CONTAINER:/etc/rancher/k3s/k3s.yaml" "$HOME/.kube/config"
sed -i 's/127.0.0.1/localhost/g' "$HOME/.kube/config"
export KUBECONFIG="$HOME/.kube/config"

echo "Waiting for node readiness..."
kubectl wait --for=condition=Ready node --all --timeout=120s
kubectl get nodes -o wide

echo "Setup complete."
echo "If needed, export: KUBECONFIG=$HOME/.kube/config"
echo "Run: cd \"$SCRIPT_DIR\" && cargo run"
