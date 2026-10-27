#!/usr/bin/env bash
# Level-4 image smoke: real tool calls through the running image, HTTP stage
# only (infra/smoke.env turns rendering off; scripts/test-render-sandbox.sh
# covers the Chromium stage under the chart's security settings).
#
#   1. `extract` reads https://www.rust-lang.org (server-rendered, behind a
#      redirect) and, in the same call, refuses a private address before any
#      request.
#   2. `crawl_company_website` of https://www.rust-lang.org with max_depth=1
#      returns more than one page.
# `search` needs SearXNG, which this container has no route to; it is
# checked after deploy, inside the cluster.
#
# Usage:  bash infra/smoke-tool.sh <base-url>     (called by run-image-smoke.sh)
# Deps:   python3 on the runner and outbound internet; nothing enters the image.
set -euo pipefail

BASE_URL="${1:?usage: smoke-tool.sh <base-url>}"

python3 - "${BASE_URL}" <<'PY'
import json
import sys
import urllib.error
import urllib.request

base_url = sys.argv[1].rstrip("/")
TIMEOUT_SECONDS = 240
SITE = "https://www.rust-lang.org"
MIN_PAGE_CHARS = 500
ERROR_BODY_LIMIT = 600


def call(tool, arguments):
    request = urllib.request.Request(  # noqa: S310 -- the smoke's own bridge URL
        f"{base_url}/tools/{tool}",
        data=json.dumps(arguments).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:  # noqa: S310
            body = json.load(response)
    except urllib.error.HTTPError as failure:
        detail = failure.read().decode("utf-8", errors="replace")[:ERROR_BODY_LIMIT]
        raise SystemExit(f"{tool}: HTTP {failure.code} from the bridge: {detail}") from None
    if body.get("isError"):
        raise SystemExit(f"{tool} failed: {json.dumps(body)[:ERROR_BODY_LIMIT]}")
    return body.get("structuredContent") or {}


extracted = call("extract", {"urls": [SITE, "http://10.0.0.1/admin"]})
pages = {page["url"]: page["text"] for page in extracted.get("extracted", [])}
failures = {failure["url"]: failure["error"] for failure in extracted.get("failed", [])}
page_chars = len(pages.get(SITE, ""))
if page_chars < MIN_PAGE_CHARS:
    raise SystemExit(f"extract read {page_chars} chars from {SITE}: {json.dumps(extracted)[:ERROR_BODY_LIMIT]}")
if failures.get("http://10.0.0.1/admin") != "refused: private address":
    raise SystemExit(f"a private address was not refused: {json.dumps(failures)}")

crawled = call("crawl_company_website", {"url": SITE, "max_depth": 1, "max_breadth": 5})
crawled_pages = len(crawled.get("extracted", []))
if crawled_pages < 2:
    raise SystemExit(f"crawl_company_website returned {crawled_pages} page(s): {json.dumps(crawled)[:ERROR_BODY_LIMIT]}")

print(f"smoke: {SITE} read over HTTP ({page_chars} chars), private address refused, crawl read {crawled_pages} pages")
PY
