# websearch MCP Server

Self-hosted web search, page extraction, crawl and research MCP server: SearXNG for search, stygian-browser for rendering

## Tools

Names, arguments and result shapes copy mcp-tavily-server's, so plans written
against tavily run unchanged after the server swap. The one addition,
`languages` on `search` and `web_research`, is optional.

| Tool | Does | Caller can rely on |
|---|---|---|
| `search(query, topic, time_range?, days?, start_date?, end_date?, languages?)` | Web search through SearXNG | At most 10 results, best first; `topic=news` merges SearXNG's news and general categories by URL and prefixes snippets with the publication date; `answer` is always empty; answers are cached for 10 minutes per (query, topic, time range, languages) within a server process, at most 1,000 at a time (the oldest is evicted). `start_date`/`end_date` are accepted and ignored. `languages` (see below) picks the languages; without it, English. |
| `extract(urls)` | Pages to readable text | Every URL lands in `extracted` or in `failed` with a typed reason. Plain HTTP first; Chromium only when HTTP finds no readable body. Non-HTML (PDF, office) fails as `unsupported content type`. At most 50 URLs and 20 MiB of text per call (rendered pages are cut to 5 MiB of HTML in the browser); the rest are reported as not read. |
| `download_file(url)` | One file | Text as text, anything else base64; at most 10 MiB. |
| `crawl(start_url, instructions?, …)` / `crawl_company_website(url, …)` | Bounded breadth-first crawl | Same site as the start URL (a redirect elsewhere does not move it) unless `allow_external`; `max_breadth` (at most 50), `max_depth` (at most 5), `limit` (at most 200; 50 for `crawl`) and a 60–300 s deadline, which also covers robots.txt and scoring, bound it; `robots.txt` is honoured; text cut to 5,000 characters unless `full_content`, and at most 20 MiB of text per crawl (past it the crawl stops and lists what it did not read); with `instructions`, pages tgi-core's fast model scores irrelevant are dropped. |
| `web_research(input, model, languages?)` | Search, read, write a report | Up to 3 searches and 8 sources (`pro`: 6 and 16); the report cites only sources that were read; every search runs in each of `languages`. Not read-only: the hub asks for approval. |
| `search_social_media(…)` | — | Always fails with `tool_not_available`. |

`languages` is a list of ISO 639-1 language codes, each optionally with a
region (`["de"]`, `["de-DE"]`, `["de", "fr", "en", "es"]`). Each language gets
its own SearXNG pass (two queries with `topic=news`), run in parallel, and
their results are interleaved one per language in turn, so every language
gets a share of the 10 hits. Codes are normalised (`DE_de` → `de-DE`) and
de-duplicated; anything not two letters plus an optional region is
`invalid_argument`, as is `"all"` (SearXNG's "all languages" gives Bing no
market at all, which is where Chinese results for "inxm" came from) and more
than `SEARCH_MAX_LANGUAGES` (default 4). The codes are not checked against
what the engines know: SearXNG accepts any code of that shape, and a country
code such as `us` or `cn` gives Bing no market just like `"all"`. The tool
descriptions say so; traces record every call's `search.languages` and the
hosts it got back (see Telemetry), so misuse shows.

Every fetch, in both stages, only reaches public http(s) addresses: private,
loopback, link-local and cluster addresses are refused on resolution and on
every redirect (in Chromium through CDP request interception), and no
credentials or cookies are carried between pages.

## Data

- **SearXNG** (`SEARXNG_URL`, default `http://mcp-websearch-server-searxng:8080`):
  upstream image, deployed by this chart (`searxng.enabled`) with a
  `settings.yml` that enables the JSON API and Brave, DuckDuckGo, Bing, Bing
  News and Google News, and runs every search without `languages` in
  `searxng.defaultLanguage` (default `en`): with SearXNG's own default, Bing
  gets no market and can answer in another language. `SEARCH_MAX_LANGUAGES`
  comes from `searxng.maxLanguages` (default 4).
- **Chromium** ships in the image and renders with its own sandbox on: the
  server runs as a non-root user (UID 10001), and with `render.enabled` the
  chart lets the pod create the user namespaces that sandbox needs (seccomp
  `Unconfined`; Chromium filters each renderer with its own seccomp policy).
  Where a runtime forbids them, Chromium does not start and pages are not
  rendered, rather than rendered unsandboxed. `RENDER_ENABLED` (default `true`),
  `RENDER_POOL_SIZE` (default 2) and `RENDER_TIMEOUT_SECS` (default 20) come
  from the chart's `render` values.
- **tgi-core** (`TGI_URL`, `DEFAULT_MODEL`) scores crawled pages and plans and
  writes research; without `TGI_URL` those calls fail as not configured.
- **Telemetry**: spans go to `OTLP_ENDPOINT` (OTLP/gRPC) when set; see
  below.

Nothing is stored: the search cache lives in the server process's memory.

## Telemetry

One span per tool call, named `tool.<tool>`, with `tool.latency_ms`. A call
that fails (refused arguments included) sets `otel.status_code=ERROR` and
`error.type`: `invalid_argument`, `upstream_failed` or `tool_not_available`.

| Span | Attributes |
|---|---|
| `tool.search` | `search.languages` (empty: the chart default), `search.cache_hit`, `search.result_count`, `search.result_hosts` (hosts of the hits, in order), `search.unresponsive_engines` |
| `search.engine_query` (child of `tool.search` and of `web_research`'s searches; one per SearXNG query) | `search.category`, `search.language`, `search.result_count`, `search.result_hosts`, `search.unresponsive_engines`; on failure `error.type` (`unreachable`, `http_status`, `decode`) |
| `tool.extract` | `extract.url_count`, `extract.stage.http` / `.render` / `.failed`: which stage answered each URL |
| `extract.page` (one per URL, also inside crawls and `web_research`) | `url.full`, `extract.stage` (`http`, `render`, `failed`), `extract.error`, `extract.render_error` |
| `tool.download_file` | `url.full` |
| `tool.crawl` (named `tool.crawl` or `tool.crawl_company_website`) | `crawl.pages_read`, `crawl.timed_out`, `crawl.scored_irrelevant`, `crawl.scoring_failures`, `crawl.text_budget_spent` |
| `tool.web_research` | `research.model`, `research.languages`, `research.source_count` |
| `tool.search_social_media` | always fails: `error.type=tool_not_available` |

To spot searches that came back in the wrong language, look for
`search.engine_query` spans whose `search.result_hosts` don't fit their
`search.language`.
