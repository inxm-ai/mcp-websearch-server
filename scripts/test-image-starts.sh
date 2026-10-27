#!/usr/bin/env bash
# Starts this repo's image the way its chart runs it, and fails when it
# can't (inxm-ai/lz-infra#101). PR CI runs it on the image it just built,
# so an image that would crash after a deploy goes red on its PR instead.
#
# Usage: bash scripts/test-image-starts.sh <image>
# Deps:  bash, docker, helm, yq (v4), jq, curl (all on GitHub's ubuntu runners)
#
# Checks, in order:
#   1. Libraries are consistent: `pip check` in the image's Python and in
#      every virtualenv in it (mismatched or missing libraries).
#   2. Init containers that use this image succeed.
#   3. The server starts and lists at least one tool (/healthz/child).
# Not checked: the exact tool list (the harness's infra/run-image-smoke.sh
# does that) and tool calls, so a library a tool only loads when it's called
# can still be missing.
#
# The chart is rendered with infra/smoke-values.yaml when it exists: values
# the deploy sets and the chart requires (a storage class, a hostname). Each
# container gets the env the chart gives it, then infra/smoke.env on top:
# dummy values for settings the chart takes from Secrets, and overrides.
# Services a container needs (a database) run on the CI runner; containers
# reach the runner as host.docker.internal.
#
# Env: STARTUP_TIMEOUT_SECONDS         how long the server may take to list
#                                      its tools (default 120)
#      HEALTH_ATTEMPT_TIMEOUT_SECONDS  how long one /healthz/child call may
#                                      take (default 10). Each call starts a
#                                      fresh server, so one with heavy imports
#                                      (torch, docling) needs more.

set -euo pipefail

IMAGE="${1:?usage: test-image-starts.sh <image>}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHART_DIR="${REPO_ROOT}/infra/helm"
SMOKE_ENV_FILE="${REPO_ROOT}/infra/smoke.env"
SMOKE_VALUES_FILE="${REPO_ROOT}/infra/smoke-values.yaml"
STARTUP_TIMEOUT_SECONDS="${STARTUP_TIMEOUT_SECONDS:-120}"
HEALTH_ATTEMPT_TIMEOUT_SECONDS="${HEALTH_ATTEMPT_TIMEOUT_SECONDS:-10}"
POLL_INTERVAL_SECONDS=2
CONTAINER_LOG_LINES=200
VENV_SEARCH_DEPTH=5

WORK_DIR="$(mktemp -d)"
SERVER_CONTAINER=""

log() { printf 'image check: %s\n' "$*" >&2; }

cleanup() {
  if [ -n "${SERVER_CONTAINER}" ]; then
    docker rm -f "${SERVER_CONTAINER}" >/dev/null 2>&1 || true
  fi
  rm -rf "${WORK_DIR}"
}
trap cleanup EXIT

fail() {
  log "FAILED: $*"
  exit 1
}

# --- the chart's view of the pod ---------------------------------------------
# Charts without appName name their Deployment with the standard fullname
# helper, which is the release name when that is the chart's own name.
APP_NAME="$(yq '.appName // ""' "${CHART_DIR}/values.yaml")"
if [ -z "${APP_NAME}" ]; then
  APP_NAME="$(yq '.name' "${CHART_DIR}/Chart.yaml")"
fi
HELM_VALUES_ARGS=()
if [ -f "${SMOKE_VALUES_FILE}" ]; then
  HELM_VALUES_ARGS=(--values "${SMOKE_VALUES_FILE}")
fi
helm template "${APP_NAME}" "${CHART_DIR}" "${HELM_VALUES_ARGS[@]}" > "${WORK_DIR}/rendered.yaml" \
  || fail "the chart doesn't render; values the deploy sets go in infra/smoke-values.yaml"
yq -o=json "select(.kind == \"Deployment\" and .metadata.name == \"${APP_NAME}\")" \
  "${WORK_DIR}/rendered.yaml" > "${WORK_DIR}/deployment.json"
[ -s "${WORK_DIR}/deployment.json" ] || fail "the chart renders no Deployment named ${APP_NAME}"
POD_SPEC="$(jq '.spec.template.spec' "${WORK_DIR}/deployment.json")"
SERVER_SPEC="$(jq --arg name "${APP_NAME}" '.containers | (map(select(.name == $name)) + .)[0]' <<<"${POD_SPEC}")"
# Init containers that run this repo's image (for example database
# migrations) are part of whether the image works; others aren't ours.
SERVER_IMAGE_IN_CHART="$(jq -r '.image' <<<"${SERVER_SPEC}")"

# Names infra/smoke.env sets: it wins over the chart, so the chart's values
# for these are left out.
SMOKE_ENV_NAMES='[]'
if [ -f "${SMOKE_ENV_FILE}" ]; then
  SMOKE_ENV_NAMES="$(grep -E '^[A-Za-z_][A-Za-z0-9_]*=' "${SMOKE_ENV_FILE}" | cut -d= -f1 | jq -R . | jq -s .)"
fi

# The chart's plain env values as NAME=value lines, minus those smoke.env
# sets, for reading MCP_BASE_PATH.
chart_env_values() {
  jq -r --argjson overridden "${SMOKE_ENV_NAMES}" \
    '.env // [] | .[] | select(has("value")) | select(.name as $name | $overridden | index($name) | not)
     | "\(.name)=\(.value)"' <<<"$1"
}

# docker run arguments that give a container what the chart gives it: its
# plain env values (as --env, since a value can span lines, which an env
# file can't hold), then infra/smoke.env, an empty directory at every volume
# mount, and its command and args.
container_run_args() {
  local spec="$1"
  jq -j --argjson overridden "${SMOKE_ENV_NAMES}" \
    '.env // [] | .[] | select(has("value")) | select(.name as $name | $overridden | index($name) | not)
     | "--env", "\u0000", "\(.name)=\(.value)", "\u0000"' <<<"${spec}"
  if [ -f "${SMOKE_ENV_FILE}" ]; then
    printf -- '--env-file\0%s\0' "${SMOKE_ENV_FILE}"
  fi
  jq -j '.volumeMounts // [] | .[] | "--tmpfs", "\u0000", .mountPath, "\u0000"' <<<"${spec}"
  printf -- '--add-host\0host.docker.internal:host-gateway\0'
}

# The image and what runs in it: k8s `command` replaces the entrypoint,
# `args` the image's CMD.
container_command() {
  local spec="$1"
  if jq -e '.command | length > 0' <<<"${spec}" >/dev/null; then
    printf -- '--entrypoint\0%s\0' "$(jq -r '.command[0]' <<<"${spec}")"
    printf '%s\0' "${IMAGE}"
    jq -j '.command[1:][], (.args // [])[] | ., "\u0000"' <<<"${spec}"
  else
    printf '%s\0' "${IMAGE}"
    jq -j '(.args // [])[] | ., "\u0000"' <<<"${spec}"
  fi
}

# Settings the chart takes from Secrets or ConfigMaps can't be read here:
# each one needs a dummy value in infra/smoke.env.
require_smoke_env_for_secret_settings() {
  local spec="$1" container="$2" missing=()
  while IFS= read -r name; do
    [ -n "${name}" ] || continue
    if [ ! -f "${SMOKE_ENV_FILE}" ] || ! grep -q "^${name}=" "${SMOKE_ENV_FILE}"; then
      missing+=("${name}")
    fi
  done < <(jq -r '.env // [] | .[] | select(has("valueFrom")) | .name' <<<"${spec}")
  if [ "${#missing[@]}" -gt 0 ]; then
    fail "${container} takes ${missing[*]} from a Secret or ConfigMap; add a dummy value for each to infra/smoke.env"
  fi
  if jq -e '.envFrom | length > 0' <<<"${spec}" >/dev/null; then
    log "${container} also reads envFrom; its settings come only from infra/smoke.env here"
  fi
}

# --- 1. libraries are consistent --------------------------------------------
# The image must already be built: docker run would otherwise try to pull it
# and fail in a way that looks like a broken image.
docker image inspect "${IMAGE}" >/dev/null 2>&1 \
  || fail "no local image ${IMAGE}; build it first (docker build -t ${IMAGE} .)"

log "checking libraries in ${IMAGE}"
PIP_CHECK_STATUS=0
docker run --rm --entrypoint sh "${IMAGE}" -c '
  if ! command -v python >/dev/null 2>&1; then
    echo "no Python in the image, nothing to check"
    exit 0
  fi
  status=0
  echo "pip check: $(command -v python)"
  python -m pip check || status=1
  for config in $(find / -xdev -maxdepth '"${VENV_SEARCH_DEPTH}"' -name pyvenv.cfg 2>/dev/null); do
    venv="$(dirname "${config}")"
    echo "pip check: ${venv}"
    python -m pip --python "${venv}/bin/python" check || status=1
  done
  exit "${status}"
' >&2 || PIP_CHECK_STATUS=$?
# 125-127 are docker run's own failures (it couldn't start the container);
# anything else is pip check reporting a problem.
if [ "${PIP_CHECK_STATUS}" -ge 125 ]; then
  fail "couldn't run ${IMAGE} to check its libraries (docker run exited ${PIP_CHECK_STATUS}, see above)"
elif [ "${PIP_CHECK_STATUS}" -ne 0 ]; then
  fail "the image has mismatched or missing libraries (see pip check above)"
fi

# --- 2. init containers that use this image ---------------------------------
INIT_COUNT="$(jq '.initContainers // [] | length' <<<"${POD_SPEC}")"
for ((index = 0; index < INIT_COUNT; index++)); do
  INIT_SPEC="$(jq ".initContainers[${index}]" <<<"${POD_SPEC}")"
  INIT_NAME="$(jq -r '.name' <<<"${INIT_SPEC}")"
  if [ "$(jq -r '.image' <<<"${INIT_SPEC}")" != "${SERVER_IMAGE_IN_CHART}" ]; then
    continue
  fi
  require_smoke_env_for_secret_settings "${INIT_SPEC}" "init container ${INIT_NAME}"
  mapfile -d '' RUN_ARGS < <(container_run_args "${INIT_SPEC}")
  mapfile -d '' COMMAND < <(container_command "${INIT_SPEC}")
  log "running init container ${INIT_NAME}"
  docker run --rm "${RUN_ARGS[@]}" "${COMMAND[@]}" >&2 \
    || fail "init container ${INIT_NAME} exited non-zero"
done

# --- 3. the server starts and lists at least one tool -----------------------
require_smoke_env_for_secret_settings "${SERVER_SPEC}" "${APP_NAME}"
SERVER_PORT="$(jq -r '.ports[0].containerPort // 8000' <<<"${SERVER_SPEC}")"
mapfile -d '' RUN_ARGS < <(container_run_args "${SERVER_SPEC}")
mapfile -d '' COMMAND < <(container_command "${SERVER_SPEC}")
SERVER_CONTAINER="$(docker run --detach --publish "127.0.0.1:0:${SERVER_PORT}" "${RUN_ARGS[@]}" "${COMMAND[@]}")"

show_server_logs() {
  log "--- ${APP_NAME} logs (last ${CONTAINER_LOG_LINES} lines) ---"
  docker logs --tail "${CONTAINER_LOG_LINES}" "${SERVER_CONTAINER}" >&2 2>&1 || true
}

HOST_PORT="$(docker port "${SERVER_CONTAINER}" "${SERVER_PORT}" | head -1 | rev | cut -d: -f1 | rev)"
# The bridge prefixes its routes with MCP_BASE_PATH; smoke.env wins over the
# chart, as it does for the container.
BASE_PATH="$({ chart_env_values "${SERVER_SPEC}"; cat "${SMOKE_ENV_FILE}" 2>/dev/null; } \
  | grep -E '^MCP_BASE_PATH=' | tail -1 | cut -d= -f2- | sed 's:/*$::')" || true
HEALTH_URL="http://127.0.0.1:${HOST_PORT}${BASE_PATH}/healthz/child"
log "waiting up to ${STARTUP_TIMEOUT_SECONDS}s for ${HEALTH_URL}"

DEADLINE=$((SECONDS + STARTUP_TIMEOUT_SECONDS))
TOOL_COUNT=""
while [ "${SECONDS}" -lt "${DEADLINE}" ]; do
  if [ "$(docker inspect --format '{{.State.Running}}' "${SERVER_CONTAINER}")" != "true" ]; then
    show_server_logs
    fail "${APP_NAME} exited before it served its tools"
  fi
  if HEALTH="$(curl --silent --fail --max-time "${HEALTH_ATTEMPT_TIMEOUT_SECONDS}" "${HEALTH_URL}")"; then
    TOOL_COUNT="$(jq -r '.tools // 0' <<<"${HEALTH}")"
    break
  fi
  sleep "${POLL_INTERVAL_SECONDS}"
done

if [ -z "${TOOL_COUNT}" ]; then
  show_server_logs
  fail "${APP_NAME} didn't answer ${HEALTH_URL} within ${STARTUP_TIMEOUT_SECONDS}s"
fi
if [ "${TOOL_COUNT}" -lt 1 ]; then
  show_server_logs
  fail "${APP_NAME} started but lists no tools"
fi
log "ok: libraries consistent, ${APP_NAME} started and lists ${TOOL_COUNT} tools"
