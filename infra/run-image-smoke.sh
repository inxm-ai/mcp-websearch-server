# Rendered from tools-create-mcp-repo/rust-harness at 9a6d487.
# Do not edit directly; update the canonical harness and re-render.

#!/usr/bin/env bash
# Image smoke engine — runs the artefact we deploy, not the checkout.
#
# Both gates call this same file: the harness image workflow at PR time and
# the service playbook between build and helm rollout. It travels with the
# repository so both paths execute identical bytes.
#
# Usage:    bash infra/run-image-smoke.sh <image-reference>
# Deps:     bash + docker + curl + jq (runner-side only; nothing here enters
#           the image)
# Env:      SMOKE_ENV_FILE         --env-file for the container; defaults to
#                                  infra/smoke.env when that file exists. The
#                                  deployment injects env via helm values, so a
#                                  bare `docker run` boots the bridge's bundled
#                                  sample — repos whose container needs env to
#                                  be ITSELF (MCP_SERVER_COMMAND at minimum)
#                                  declare it there, kept in sync with the
#                                  chart's values.
#           SMOKE_STARTUP_TIMEOUT  seconds to wait for /healthz/child (default 60)
#           SMOKE_HEALTH_TIMEOUT   seconds per /healthz/child attempt (default 5)
#
# Levels — each one an observable guarantee, reported as the highest achieved:
#   2  the container serves /healthz/child 200: child boots and lists tools
#   3  GET /tools names == infra/expected-tools.txt: OUR tools, not a sample
#   4  infra/smoke-tool.sh <base-url> exits 0: one real call did the job
#
# Required level: 3 always (expected-tools.txt missing = fail, never skip);
# 4 exactly when infra/smoke-tool.sh exists — its presence IS the level-4
# declaration, and deleting it is a visible diff, never a silent fallback.
#
# Output contract: diagnostics on stderr; the LAST stdout line, printed after
# cleanup, is `SMOKE_LEVEL=<n> RESULT=<pass|fail>`. Callers must treat a
# missing status line as failure. Exit 0 iff RESULT=pass.

set -u

IMAGE_REFERENCE="${1:?usage: run-image-smoke.sh <image-reference>}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXPECTED_TOOLS_FILE="${REPO_ROOT}/infra/expected-tools.txt"
TOOL_SMOKE_HOOK="${REPO_ROOT}/infra/smoke-tool.sh"
STARTUP_TIMEOUT_SECONDS="${SMOKE_STARTUP_TIMEOUT:-60}"
HEALTH_CHECK_TIMEOUT_SECONDS="${SMOKE_HEALTH_TIMEOUT:-5}"
HEALTH_POLL_INTERVAL_SECONDS=2
BRIDGE_CONTAINER_PORT=8000

CONTAINER_ID=""
ACHIEVED_LEVEL=0
RESULT="fail"

log() { printf 'smoke: %s\n' "$*" >&2; }

finish() {
    if [ -n "${CONTAINER_ID}" ]; then
        if [ "${RESULT}" != "pass" ]; then
            log "--- container logs (last 200 lines) ---"
            docker logs --tail 200 "${CONTAINER_ID}" >&2 2>&1 || true
        fi
        docker rm -f "${CONTAINER_ID}" >/dev/null 2>&1 || true
    fi
    # The status line is the machine-readable outcome, emitted after cleanup
    # so a wedged teardown cannot leave a stale pass on the wire.
    printf 'SMOKE_LEVEL=%s RESULT=%s\n' "${ACHIEVED_LEVEL}" "${RESULT}"
    [ "${RESULT}" = "pass" ] && exit 0 || exit 1
}
trap finish EXIT

# --- start the container with its real entrypoint --------------------------
# Random host port + tracked by ID: tenant deployments run concurrently, so
# fixed ports or container names would collide.
DOCKER_RUN_ARGS=(run --detach --publish "127.0.0.1:0:${BRIDGE_CONTAINER_PORT}")
ENV_FILE="${SMOKE_ENV_FILE:-${REPO_ROOT}/infra/smoke.env}"
if [ -f "${ENV_FILE}" ]; then
    log "container env from ${ENV_FILE#"${REPO_ROOT}"/}"
    DOCKER_RUN_ARGS+=(--env-file "${ENV_FILE}")
fi
CONTAINER_ID="$(docker "${DOCKER_RUN_ARGS[@]}" "${IMAGE_REFERENCE}")" || {
    log "docker run failed for ${IMAGE_REFERENCE}"
    CONTAINER_ID=""
    exit 1
}

HOST_PORT="$(docker port "${CONTAINER_ID}" "${BRIDGE_CONTAINER_PORT}" | head -1 | rev | cut -d: -f1 | rev)"
if [ -z "${HOST_PORT}" ]; then
    log "could not resolve the published host port"
    exit 1
fi
# The bridge prefixes every route when MCP_BASE_PATH is set, and the env file
# is where a repo declares its deployed environment — so the prefix is derived
# from the SAME file the container booted with, never maintained separately.
BASE_PATH=""
if [ -f "${ENV_FILE}" ]; then
    BASE_PATH="$(grep -E '^MCP_BASE_PATH=' "${ENV_FILE}" | tail -1 | cut -d= -f2- | tr -d '"' | sed 's:/*$::')"
fi
BASE_URL="http://127.0.0.1:${HOST_PORT}${BASE_PATH}"
log "container ${CONTAINER_ID:0:12} of ${IMAGE_REFERENCE} on ${BASE_URL}"

# --- level 2: the child boots and lists tools ------------------------------
# The deadline is only ever observed BETWEEN attempts, so each attempt's own
# --max-time is clamped to the time actually left: an unclamped
# SMOKE_HEALTH_TIMEOUT could otherwise let a single in-flight attempt run past
# SMOKE_STARTUP_TIMEOUT before this loop notices.
DEADLINE=$((SECONDS + STARTUP_TIMEOUT_SECONDS))
while true; do
    REMAINING_SECONDS=$((DEADLINE - SECONDS))
    if [ "${REMAINING_SECONDS}" -le 0 ]; then
        log "level 2 FAILED: /healthz/child not 200 within ${STARTUP_TIMEOUT_SECONDS}s"
        exit 1
    fi
    ATTEMPT_TIMEOUT_SECONDS=${REMAINING_SECONDS}
    if [ "${ATTEMPT_TIMEOUT_SECONDS}" -gt "${HEALTH_CHECK_TIMEOUT_SECONDS}" ]; then
        ATTEMPT_TIMEOUT_SECONDS=${HEALTH_CHECK_TIMEOUT_SECONDS}
    fi
    curl --silent --fail --max-time "${ATTEMPT_TIMEOUT_SECONDS}" "${BASE_URL}/healthz/child" >/dev/null 2>&1 && break
    sleep "${HEALTH_POLL_INTERVAL_SECONDS}"
done
ACHIEVED_LEVEL=2
log "level 2 ok: child healthy"

# --- level 3: the advertised tools are THIS repo's tools -------------------
if [ ! -f "${EXPECTED_TOOLS_FILE}" ]; then
    log "level 3 FAILED: ${EXPECTED_TOOLS_FILE#"${REPO_ROOT}"/} is missing."
    log "  List this server's tool names there, one per line — it is the"
    log "  contract that the image serves OUR tools and not a bundled sample."
    exit 1
fi
ACTUAL_TOOLS="$(curl --silent --fail --max-time 15 "${BASE_URL}/tools")" || {
    log "level 3 FAILED: GET /tools did not return 200"
    exit 1
}
EXPECTED_SORTED="$(grep -v -E '^\s*(#|$)' "${EXPECTED_TOOLS_FILE}" | sort -u)"
ACTUAL_SORTED="$(printf '%s' "${ACTUAL_TOOLS}" | jq -r '.[].name' 2>/dev/null | sort -u)" || {
    log "level 3 FAILED: /tools response was not the expected JSON shape"
    exit 1
}
TOOL_DIFF="$(comm -3 <(printf '%s\n' "${EXPECTED_SORTED}") <(printf '%s\n' "${ACTUAL_SORTED}") | while IFS=$'\t' read -r missing unexpected; do
    if [ -n "${missing}" ]; then printf 'missing:    %s\n' "${missing}"; fi
    if [ -n "${unexpected}" ]; then printf 'unexpected: %s\n' "${unexpected}"; fi
done)"
if [ -n "${TOOL_DIFF}" ]; then
    log "level 3 FAILED: served tools differ from infra/expected-tools.txt:"
    printf '%s\n' "${TOOL_DIFF}" >&2
    exit 1
fi
ACHIEVED_LEVEL=3
log "level 3 ok: expected tools served"

# --- level 4: one real tool call, iff the repo declared one ----------------
if [ -f "${TOOL_SMOKE_HOOK}" ]; then
    if bash "${TOOL_SMOKE_HOOK}" "${BASE_URL}"; then
        ACHIEVED_LEVEL=4
        log "level 4 ok: tool smoke passed"
    else
        log "level 4 FAILED: ${TOOL_SMOKE_HOOK#"${REPO_ROOT}"/} exited non-zero."
        log "  The hook's presence declares level 4 — fix the smoke or the"
        log "  image; deleting the hook to pass is a reviewable diff."
        exit 1
    fi
else
    log "no infra/smoke-tool.sh — level 3 is this repo's declared ceiling"
fi

RESULT="pass"
exit 0
