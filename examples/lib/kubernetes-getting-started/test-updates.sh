#!/usr/bin/env bash
set -euo pipefail

echo "Creating drasi-demo ConfigMap..."
kubectl -n default apply -f - <<'YAML'
apiVersion: v1
kind: ConfigMap
metadata:
  name: drasi-demo
data:
  color: red
YAML

sleep 2

echo "Updating drasi-demo ConfigMap color=blue..."
kubectl -n default patch configmap drasi-demo --type merge -p '{"data":{"color":"blue"}}'

sleep 2

echo "Deleting drasi-demo ConfigMap..."
kubectl -n default delete configmap drasi-demo --ignore-not-found
