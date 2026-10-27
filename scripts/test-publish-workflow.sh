#!/usr/bin/env bash
# Checks the publish wiring holds what its PR relies on: nothing publishes
# before ci.yaml and harness.yaml pass, every registry gets the one build,
# GITHUB_TOKEN and OIDC are the only credentials, and the deploy keeps using
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
for job in push-ghcr push-acr push-ecr; do
  [[ "$(q ".jobs.\"${job}\".needs")" == "build" ]] || fail "${job} must need build (and so the checks)"
done
[[ "$(q '.jobs | keys | sort | join(",")')" == "build,ci,harness,push-acr,push-ecr,push-ghcr" ]] \
  || fail "unexpected jobs: $(q '.jobs | keys | join(",")')"

# Least privilege: read-only by default, one write scope per push job.
[[ "$(q '.permissions | to_entries | map(.key + "=" + .value) | join(",")')" == "contents=read" ]] \
  || fail "top-level permissions must be contents: read only"
[[ "$(q '.jobs."push-ghcr".permissions | to_entries | map(.key + "=" + .value) | join(",")')" == "packages=write" ]] \
  || fail "push-ghcr may only write packages"
for job in push-acr push-ecr; do
  [[ "$(q ".jobs.\"${job}\".permissions | to_entries | map(.key + \"=\" + .value) | join(\",\")")" == "id-token=write" ]] \
    || fail "${job} may only request an OIDC token"
done

# This repo is public: org secrets never reach it, so none may be referenced.
if grep -nE 'secrets\.' "${WORKFLOW}"; then
  fail "no secrets: publish with GITHUB_TOKEN (github.token) and OIDC only"
fi

[[ "$(q '.jobs."push-acr".uses')" == "inxm-ai/lz-azure/.github/workflows/push-to-acr.yml@main" ]] \
  || fail "push-acr must use lz-azure's push-to-acr.yml"
[[ "$(q '.jobs."push-ecr".uses')" == "inxm-ai/lz-aws/.github/workflows/push-to-ecr.yml@main" ]] \
  || fail "push-ecr must use lz-aws's push-to-ecr.yml"
for job in push-acr push-ecr; do
  [[ "$(q ".jobs.\"${job}\".with.\"image-name\"")" == "mcp-websearch-server" ]] || fail "${job} pushes the wrong image name"
  [[ "$(q ".jobs.\"${job}\".with.\"chart-artifact\"")" == "chart" ]] || fail "${job} must push the chart"
  [[ "$(q ".jobs.\"${job}\".with.\"image-artifact\"")" == "image" ]] || fail "${job} must push the image"
done

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

echo "publish workflow checks passed"
