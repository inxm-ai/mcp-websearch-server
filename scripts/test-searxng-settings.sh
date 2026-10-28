#!/usr/bin/env bash
# The SearXNG settings the chart renders. Every search must run in a set
# language: the tools take none, and with SearXNG's own default (auto, which
# falls back to all without an Accept-Language) Bing gets no market and
# answered "inxm" in Chinese.
set -euo pipefail

CHART_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../infra/helm" && pwd)"

settings() {
  helm template mcp-websearch-server "${CHART_DIR}" "$@" \
    --show-only templates/searxng-configmap.yaml | yq -r '.data["settings.yml"]'
}

test "$(settings | yq -r '.search.default_lang')" = "en"
test "$(settings --set searxng.defaultLanguage=de-DE | yq -r '.search.default_lang')" = "de-DE"
test "$(settings | yq -r '.search.formats | join(",")')" = "html,json"

if helm template mcp-websearch-server "${CHART_DIR}" --set searxng.defaultLanguage= > /dev/null 2>&1; then
  echo "expected an empty searxng.defaultLanguage to fail" >&2
  exit 1
fi

# app-mcp-manager registers every Service in the namespace whose name starts
# with mcp- as an MCP server; only the server itself may.
rendered="$(helm template mcp-websearch-server "${CHART_DIR}")"
services="$(yq -r 'select(.kind == "Service") | .metadata.name' <<<"${rendered}" | sed '/^---$/d')"
test "$(grep -c '^mcp-' <<<"${services}")" = "1"
grep -qx 'mcp-websearch-server' <<<"${services}"
grep -qx 'ext-mcp-websearch-server-searxng' <<<"${services}"
# The server is pointed at that Service.
test "$(yq -r 'select(.kind == "Deployment" and .metadata.name == "mcp-websearch-server")
  | .spec.template.spec.containers[0].env[] | select(.name == "SEARXNG_URL") | .value' <<<"${rendered}")" \
  = "http://ext-mcp-websearch-server-searxng:8080"

echo "mcp-websearch-server SearXNG settings checks passed"
