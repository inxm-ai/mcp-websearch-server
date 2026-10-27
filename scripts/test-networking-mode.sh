#!/usr/bin/env bash
set -euo pipefail

CHART_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../infra/helm" && pwd)"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "${WORK_DIR}"' EXIT

helm lint "${CHART_DIR}"

helm template mcp-websearch-server-ingress "${CHART_DIR}" \
  --set networking.mode=ingress > "${WORK_DIR}/ingress.yaml"
helm template mcp-websearch-server-gateway "${CHART_DIR}" \
  --set networking.mode=gateway \
  --set networking.gateway.hostname=orchestrator.inxm.ai > "${WORK_DIR}/gateway.yaml"

test "$(grep -c '^kind: HTTPRoute$' "${WORK_DIR}/ingress.yaml" || true)" -eq 0
test "$(grep -c '^kind: HTTPRoute$' "${WORK_DIR}/gateway.yaml" || true)" -eq 1
grep -q 'value: "/api/mcp-websearch-server"' "${WORK_DIR}/gateway.yaml"
grep -q 'name: mcp-websearch-server' "${WORK_DIR}/gateway.yaml"
grep -q 'port: 8000' "${WORK_DIR}/gateway.yaml"
test "$(grep -c '^        - type: ExternalAuth$' "${WORK_DIR}/gateway.yaml" || true)" -eq 1
grep -q 'name: iam-oauth2-proxy' "${WORK_DIR}/gateway.yaml"
grep -q 'port: 4180' "${WORK_DIR}/gateway.yaml"
grep -q 'path: /oauth2/auth' "${WORK_DIR}/gateway.yaml"
grep -q 'X-Auth-Request-User' "${WORK_DIR}/gateway.yaml"
grep -q 'X-Auth-Request-Email' "${WORK_DIR}/gateway.yaml"
grep -q 'X-Auth-Request-Groups' "${WORK_DIR}/gateway.yaml"
grep -q 'X-Auth-Request-Access-Token' "${WORK_DIR}/gateway.yaml"
grep -q 'X-Forwarded-Access-Token' "${WORK_DIR}/gateway.yaml"

if helm template mcp-websearch-server-invalid "${CHART_DIR}" \
  --set networking.mode=gateway > /dev/null 2>&1; then
  echo "expected gateway mode without a hostname to fail" >&2
  exit 1
fi

echo "mcp-websearch-server networking mode checks passed"
