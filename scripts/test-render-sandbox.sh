#!/usr/bin/env bash
# Renders a JavaScript page in this repo's image with Chromium's sandbox on,
# under the security settings the chart gives the pod
# (infra/helm/templates/deployment.yaml: the image's non-root user, all
# capabilities dropped, no privilege escalation, seccomp Unconfined while
# render.enabled), and fails unless:
#
#   1. https://inxm.ai, whose HTML holds no readable text, comes back with
#      text, which only a render produces;
#   2. every Chromium renderer that ran was sandboxed: the image's user, in
#      its own user namespace, under a seccomp filter, with no-new-privileges
#      and without --no-sandbox;
#   3. under the runtime's default seccomp profile, where the sandbox cannot
#      start, nothing renders: Chromium fails closed instead of running
#      unsandboxed.
#
# Usage: bash scripts/test-render-sandbox.sh <image>
# Deps:  bash, docker, curl, python3; outbound internet.

set -euo pipefail

IMAGE="${1:?usage: test-render-sandbox.sh <image>}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BRIDGE_PORT=8000
BASE_PATH=/api/mcp-websearch-server
JS_PAGE=https://inxm.ai
# inxm.ai's HTML holds a "Skip to content" link and nothing else readable.
MIN_RENDERED_CHARS=1000
STARTUP_TIMEOUT_SECONDS=60
EXTRACT_TIMEOUT_SECONDS=120
# Mirrors the chart's securityContext; keep the two in step.
CHART_SECURITY=(--cap-drop ALL --security-opt no-new-privileges)
SANDBOX_SECCOMP=(--security-opt seccomp=unconfined)
WORK_DIR="$(mktemp -d)"
CONTAINERS=()

log() { printf 'render sandbox: %s\n' "$*" >&2; }
fail() { log "FAILED: $*"; exit 1; }
cleanup() {
  for container in "${CONTAINERS[@]}"; do docker rm -f "${container}" >/dev/null 2>&1 || true; done
  rm -rf "${WORK_DIR}"
}
trap cleanup EXIT

# Starts the image with `$@` as extra docker options and prints its base URL.
start() {
  local container port
  container="$(docker run --detach --publish "127.0.0.1:0:${BRIDGE_PORT}" "${CHART_SECURITY[@]}" "$@" \
    --env MCP_SERVER_COMMAND=/usr/local/bin/mcp-websearch --env MCP_BASE_PATH="${BASE_PATH}" \
    --env RENDER_ENABLED=true --env RENDER_POOL_SIZE=1 --env RENDER_TIMEOUT_SECS=30 "${IMAGE}")"
  CONTAINERS+=("${container}")
  port="$(docker port "${container}" "${BRIDGE_PORT}" | head -1 | rev | cut -d: -f1 | rev)"
  local base="http://127.0.0.1:${port}${BASE_PATH}"
  local deadline=$((SECONDS + STARTUP_TIMEOUT_SECONDS))
  until curl --silent --fail --max-time 5 "${base}/healthz/child" >/dev/null; do
    [ "${SECONDS}" -lt "${deadline}" ] || { docker logs --tail 100 "${container}" >&2; fail "the server did not start"; }
    sleep 2
  done
  printf '%s %s\n' "${container}" "${base}"
}

# Characters of text `extract` returned for JS_PAGE.
extracted_chars() {
  python3 - "$1" "${JS_PAGE}" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))
pages = (body.get("structuredContent") or {}).get("extracted", [])
print(sum(len(page["text"]) for page in pages if page["url"] == sys.argv[2]))
PY
}

# --- 1 and 2: the chart's settings render, and every renderer is sandboxed --
read -r CONTAINER BASE_URL < <(start "${SANDBOX_SECCOMP[@]}")
log "rendering ${JS_PAGE} in ${CONTAINER:0:12}"
curl --silent --max-time "${EXTRACT_TIMEOUT_SECONDS}" -X POST -H 'content-type: application/json' \
  -d "{\"urls\": [\"${JS_PAGE}\"]}" "${BASE_URL}/tools/extract" > "${WORK_DIR}/sandboxed.json" &
EXTRACT_PID=$!
: > "${WORK_DIR}/renderers.txt"
while kill -0 "${EXTRACT_PID}" 2>/dev/null; do
  docker exec "${CONTAINER}" sh -c '
    # Anchored on the binary: an unanchored pattern also matches this shell.
    for pid in $(pgrep -f -- "^/usr/lib/chromium/chromium --type=renderer"); do
      status=/proc/${pid}/status
      printf "%s uid=%s seccomp=%s nnp=%s ownns=%s nosandbox=%s\n" "${pid}" \
        "$(awk "/^Uid:/{print \$2}" ${status})" "$(awk "/^Seccomp:/{print \$2}" ${status})" \
        "$(awk "/^NoNewPrivs:/{print \$2}" ${status})" \
        "$( [ "$(readlink /proc/${pid}/ns/user)" != "$(readlink /proc/1/ns/user)" ] && echo yes || echo no)" \
        "$(tr "\0" " " < /proc/${pid}/cmdline | grep -c -- --no-sandbox)"
    done' >> "${WORK_DIR}/renderers.txt" 2>/dev/null || true
  sleep 1
done
wait "${EXTRACT_PID}" || fail "extract did not answer"
CHARS="$(extracted_chars "${WORK_DIR}/sandboxed.json")"
[ "${CHARS}" -ge "${MIN_RENDERED_CHARS}" ] || fail "${JS_PAGE} was not rendered (${CHARS} chars): $(head -c 600 "${WORK_DIR}/sandboxed.json")"
[ -s "${WORK_DIR}/renderers.txt" ] || fail "no Chromium renderer was seen while rendering"
EXPECTED_UID="$(docker exec "${CONTAINER}" id -u)"
UNSANDBOXED="$(sort -u "${WORK_DIR}/renderers.txt" | grep -v -E "uid=${EXPECTED_UID} seccomp=2 nnp=1 ownns=yes nosandbox=0$" || true)"
[ -z "${UNSANDBOXED}" ] || fail "renderers ran unsandboxed:
${UNSANDBOXED}"
log "ok: ${JS_PAGE} rendered (${CHARS} chars); $(sort -u -t' ' -k1,1 "${WORK_DIR}/renderers.txt" | wc -l | tr -d ' ') renderer(s) sandboxed (uid ${EXPECTED_UID}, own user namespace, seccomp filter, no-new-privileges)"

# --- 3: without the sandbox's syscalls, nothing renders -----------------------
read -r CONTAINER BASE_URL < <(start)
curl --silent --max-time "${EXTRACT_TIMEOUT_SECONDS}" -X POST -H 'content-type: application/json' \
  -d "{\"urls\": [\"${JS_PAGE}\"]}" "${BASE_URL}/tools/extract" > "${WORK_DIR}/default-seccomp.json" \
  || fail "extract did not answer under the default seccomp profile"
CHARS="$(extracted_chars "${WORK_DIR}/default-seccomp.json")"
[ "${CHARS}" -lt "${MIN_RENDERED_CHARS}" ] || fail "${JS_PAGE} rendered under the default seccomp profile (${CHARS} chars): Chromium ran without its sandbox"
log "ok: under the default seccomp profile Chromium does not start (${CHARS} chars, HTTP text only)"
