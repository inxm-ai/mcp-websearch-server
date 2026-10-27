#!/usr/bin/env bash
# Checks the publish wiring holds what its PR relies on: nothing publishes
# before ci.yaml and harness.yaml pass, GHCR gets the build's own digest and
# the chart, GITHUB_TOKEN is the only credential, and the deploy keeps using
# the published image instead of building its own.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKFLOW="${ROOT}/.github/workflows/build-and-publish.yaml"
PLAYBOOK="${ROOT}/infra/playbook.yaml"

fail() { echo "publish workflow check: $*" >&2; exit 1; }
q() { yq -r "$1" "${WORKFLOW}"; }

[[ "$(q '.jobs.ci.uses')" == "./.github/workflows/ci.yaml" ]] || fail "ci job must call ci.yaml"
[[ "$(q '.jobs.harness.uses')" == "./.github/workflows/harness.yaml" ]] || fail "harness job must call harness.yaml"
[[ "$(q '.jobs.build.needs | sort | join(",")')" == "ci,harness" ]] || fail "build must need ci and harness"
[[ "$(q '.jobs."push-ghcr".needs')" == "build" ]] || fail "push-ghcr must need build (and so the checks)"
[[ "$(q '.jobs | keys | sort | join(",")')" == "build,ci,harness,push-ghcr" ]] \
  || fail "unexpected jobs: $(q '.jobs | keys | join(",")')"

# Least privilege: read-only by default, only packages written.
[[ "$(q '.permissions | to_entries | map(.key + "=" + .value) | join(",")')" == "contents=read" ]] \
  || fail "top-level permissions must be contents: read only"
[[ "$(q '.jobs."push-ghcr".permissions | to_entries | map(.key + "=" + .value) | join(",")')" == "packages=write" ]] \
  || fail "push-ghcr may only write packages"

# This repo is public: org secrets never reach it, so none may be referenced.
if grep -nE 'secrets\.' "${WORKFLOW}"; then
  fail "no secrets: publish with GITHUB_TOKEN (github.token) only"
fi

# A public repo can only call reusable workflows from public repos; lz-aws
# and lz-azure are internal, so their copies come from their own sync
# workflows. Only this repo's own workflows may be called.
if q '.jobs[] | select(has("uses")) | .uses' | grep -v '^\./\.github/workflows/'; then
  fail "only this repo's workflows may be called as reusable workflows"
fi

# The chart goes to GHCR, where the sync workflows pull it from.
# shellcheck disable=SC2016 # a literal workflow expression, not a shell one
grep -Fq 'oci://ghcr.io/${{ github.repository_owner }}/charts' "${WORKFLOW}" || fail "chart must be pushed to GHCR"

# enterprise-mcp-bridge's tags: latest and sha-<short> on main, the tag on a tag push.
tags="$(q '.jobs.build.steps[] | select(.id == "meta") | .with.tags')"
grep -Fq 'type=raw,value=latest,enable={{is_default_branch}}' <<<"${tags}" || fail "missing latest tag on main"
grep -Fq 'type=sha,prefix=sha-,format=short,enable={{is_default_branch}}' <<<"${tags}" || fail "missing sha-<short> tag on main"
grep -Fq 'type=ref,event=tag' <<<"${tags}" || fail "missing git tag"

# The GHCR copy must keep the build's digest.
grep -Fq 'skopeo copy --all --preserve-digests' "${WORKFLOW}" || fail "GHCR push must copy the archive digest-preserving"

# The deploy uses the published image: no build or push in the playbook.
if grep -nE 'docker (build|push)' "${PLAYBOOK}"; then
  fail "infra/playbook.yaml must deploy the published image, not build one"
fi
grep -Fq 'image.tag={{ extra_docker_image_tag }}' "${PLAYBOOK}" || fail "playbook must pin the commit's image tag"
# Only a commit SHA rolls the pod by itself; latest (explicit or not) needs the restart.
grep -Fq "is match('^[0-9a-f]{40}\$')" "${PLAYBOOK}" || fail "playbook must restart unless the tag is a commit SHA"

echo "publish workflow checks passed"
