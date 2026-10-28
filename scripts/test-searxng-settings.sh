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

echo "mcp-websearch-server SearXNG settings checks passed"
