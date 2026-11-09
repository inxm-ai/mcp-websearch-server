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

# INXM's shared space (RFC-0024): its own namespace and Gateway, no sign-in
# proxy, no disk, no autoscaler, SearXNG's security settings and the shared
# node pool on both pods.
helm template mcp-websearch-server "${CHART_DIR}" \
  --namespace inxm-shared-services \
  --set namespace=inxm-shared-services \
  --set networking.mode=gateway \
  --set networking.gateway.hostname=services.inxm.cloud \
  --set 'networking.gateway.parentRefs[0].name=shared' \
  --set networking.gateway.externalAuth=false \
  --set persistence.enabled=false \
  --set autoscaling.enabled=false \
  --set render.enabled=false \
  --set searxng.podSecurityContext.runAsNonRoot=true \
  --set searxng.securityContext.allowPrivilegeEscalation=false \
  --set nodeSelector.pool=shared \
  --set 'tolerations[0].key=pool' > "${WORK_DIR}/shared-space.yaml"
test "$(grep -c -E '^kind: (PersistentVolumeClaim|HorizontalPodAutoscaler)$' "${WORK_DIR}/shared-space.yaml" || true)" -eq 0
test "$(grep -c 'ExternalAuth' "${WORK_DIR}/shared-space.yaml" || true)" -eq 0
test "$(grep -c 'namespace: orchestrator' "${WORK_DIR}/shared-space.yaml" || true)" -eq 0
test "$(grep -c '^  namespace: inxm-shared-services$' "${WORK_DIR}/shared-space.yaml" || true)" -eq 2
test "$(grep -c 'Unconfined' "${WORK_DIR}/shared-space.yaml" || true)" -eq 0
test "$(grep -c 'pool: shared' "${WORK_DIR}/shared-space.yaml" || true)" -eq 2
test "$(grep -c 'key: pool' "${WORK_DIR}/shared-space.yaml" || true)" -eq 2
grep -q 'allowPrivilegeEscalation: false' "${WORK_DIR}/shared-space.yaml"
# Customer installs keep the orchestrator namespace and render no placement
# of their own.
grep -q 'namespace: orchestrator' "${WORK_DIR}/gateway.yaml"
test "$(grep -c -E 'nodeSelector|tolerations|affinity' "${WORK_DIR}/gateway.yaml" || true)" -eq 0

if helm template mcp-websearch-server-invalid "${CHART_DIR}" \
  --set networking.mode=gateway > /dev/null 2>&1; then
  echo "expected gateway mode without a hostname to fail" >&2
  exit 1
fi

echo "mcp-websearch-server networking mode checks passed"
