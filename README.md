# websearch MCP Server

Self-hosted web search, page extraction, crawl and research MCP server: SearXNG for search, stygian-browser for rendering

## Tools

Names, arguments and result shapes copy mcp-tavily-server's, so plans written
against tavily run unchanged after the server swap.

| Tool | Does | Caller can rely on |
|---|---|---|
| `search(query, topic, time_range?, days?, start_date?, end_date?)` | Web search through SearXNG | At most 10 results, best first; `topic=news` merges SearXNG's news and general categories by URL and prefixes snippets with the publication date; `answer` is always empty; answers are cached for 10 minutes per (query, topic, time range) within a server process. `start_date`/`end_date` are accepted and ignored. |
| `extract(urls)` | Pages to readable text | Every URL lands in `extracted` or in `failed` with a typed reason. Plain HTTP first; Chromium only when HTTP finds no readable body. Non-HTML (PDF, office) fails as `unsupported content type`. At most 50 URLs are read per call; the rest are reported as not read. |
| `download_file(url)` | One file | Text as text, anything else base64; at most 10 MiB. |
| `crawl(start_url, instructions?, …)` / `crawl_company_website(url, …)` | Bounded breadth-first crawl | Same site unless `allow_external`; `max_breadth`, `max_depth`, `limit` (50 for `crawl`) and a 60–300 s deadline bound it; `robots.txt` is honoured; text cut to 5,000 characters unless `full_content`; with `instructions`, pages tgi-core's fast model scores irrelevant are dropped. |
| `web_research(input, model)` | Search, read, write a report | Up to 3 searches and 8 sources (`pro`: 6 and 16); the report cites only sources that were read. Not read-only: the hub asks for approval. |
| `search_social_media(…)` | — | Always fails with `tool_not_available`. |

Every fetch, in both stages, only reaches public http(s) addresses: private,
loopback, link-local and cluster addresses are refused on resolution and on
every redirect (in Chromium through CDP request interception), and no
credentials or cookies are carried between pages.

## Data

- **SearXNG** (`SEARXNG_URL`, default `http://mcp-websearch-server-searxng:8080`):
  upstream image, deployed by this chart (`searxng.enabled`) with a
  `settings.yml` that enables the JSON API and Brave, DuckDuckGo, Bing, Bing
  News and Google News.
- **Chromium** ships in the image. `RENDER_ENABLED` (default `true`),
  `RENDER_POOL_SIZE` (default 2) and `RENDER_TIMEOUT_SECS` (default 20) come
  from the chart's `render` values.
- **tgi-core** (`TGI_URL`, `DEFAULT_MODEL`) scores crawled pages and plans and
  writes research; without `TGI_URL` those calls fail as not configured.
- **Telemetry**: spans go to `OTLP_ENDPOINT` when set.

Nothing is stored: the search cache lives in the server process's memory.
