#!/usr/bin/env bash
set -euo pipefail

echo "=== Kubernetes cluster info ==="
kubectl cluster-info

echo
echo "=== Nodes ==="
kubectl get nodes -o wide

echo
echo "=== Demo ConfigMap ==="
kubectl -n default get configmap drasi-demo -o yaml || echo "ConfigMap drasi-demo not found"
