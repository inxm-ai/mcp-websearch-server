<!--
Seeded from tools-create-mcp-repo/rust-harness at 9a6d487.
Repo-owned from here on: edit it freely. The renderer reports when the canonical seed has moved on, but never rewrites this file.
-->

# Repository guide

This repository exposes MCP tools through `enterprise-mcp-bridge`, built with
the official Rust MCP SDK (`rmcp`).

<!-- PLACEHOLDER: replace with this repo's auth class. Most MCPs are
pass-through adapters (keep the paragraph below); a repo that owns protected
local resources needs an explicit authorized-request witness type instead —
see RFC-0008. -->
**Auth class: pass-through adapter.** The bridge mints and injects tokens
(RFC-0008); the downstream resource server authorizes them. Tool code never
handles auth.

## Commands

| Task | Command |
|---|---|
| Build | `cargo build` |
| All sensors, terse summary — run before wrapping up | `make harness` |
| Test | `cargo test --workspace --all-targets --all-features` |
| Lint and format check | `cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings` |
| Coverage (reported signal, never a gate) | `cargo llvm-cov --workspace --all-features --summary-only` |
| Refresh the lockfile after changing dependencies | `cargo generate-lockfile` |

Sensor failure recipe: `make harness` prints one line per sensor; a failing
sensor's full output is in `.harness/<sensor>.log`. Fix the finding —
suppressions are the exception, not the fix (see `#[allow(...)]` conventions
below).

## Who owns which file

Files carrying a `Rendered from tools-create-mcp-repo/rust-harness` header
are fleet-managed: `Makefile`, `tests/typed_tool_returns.rs`,
`.github/workflows/harness.yaml`, `infra/run-image-smoke.sh`. Editing them
here is lost on the next render — change the canonical harness and
re-render instead. The harness keys in `Cargo.toml` (`[lints.rust]`,
`[lints.clippy]`, the `syn`/`walkdir` dev-dependencies) are managed the same
way, key by key; everything else in that file is this repo's.

Repo-owned: this file, `.github/workflows/ci.yaml` (build, chart and
deployment checks), `infra/expected-tools.txt`, optional
`infra/smoke.env`/`infra/smoke-tool.sh`, and all application code. A
deterministic sensor runs in `harness.yaml` and nowhere else — never
duplicate one into `ci.yaml`.

## Bridge version

The `Dockerfile` pins the bridge this server builds on (`latest`, by
digest), and PR CI checks the image on exactly that bridge
(`scripts/test-image-starts.sh`, inxm-ai/lz-infra#101). Dependabot
(`.github/dependabot.yml`) opens a PR when the digest behind `latest` moves;
merge it when the `image` check is green.

- Keep the `FROM` reference literal, with the tag `latest` (the Python 3.11
  build). Dependabot doesn't read ARG variables, and it moves a `-3.11` tag
  to `-3.12`.
- Don't add a build argument that changes the bridge registry: this repo's
  deploy builds on GHCR.

## Constraints

<!-- PLACEHOLDER: replace with this repo's real invariants as they emerge.
Write them as testable statements ("never log X", "the only supported mode
is Y"), not process narration. The invariants below hold for every repo
generated from the template. -->

- Keep `src/main.rs` to imports and startup only; tools, contracts, and
  wiring live in the rest of `src/`.
- **stdout is the JSON-RPC wire.** The stdio transport reads and writes
  framed JSON-RPC on stdin/stdout; any stray `println!`/`print!` corrupts
  every message after it. `main.rs` initializes `tracing` to write to
  stderr — use `tracing::info!`/`warn!`/`error!` everywhere, never `println!`.
  The `print_stdout` clippy lint (harness-managed, warned locally, denied by
  `make harness`/CI) is a tripwire for the common shape, not a proof — it
  cannot see output reached through an alias or a third-party crate.
- **Every tool returns `Json<T>`.** Wrap the typed contract result in
  `rmcp::handler::server::wrapper::Json` (directly, or inside
  `Result<Json<T>, McpError>`) so the tool result carries a schema'd,
  structured payload instead of an opaque string. The
  `tests/typed_tool_returns.rs` fitness function enforces this by scanning
  every `#[tool(...)]`-annotated method.
- **Modules are pure by default.** A module earns the right to do I/O —
  filesystem, network, subprocess, environment — by being named
  `*_adapter.rs`; `config.rs` is the one other exception, since it loads its
  own defaults. When a pure module needs any of those, move the access into
  an adapter and pass the data in. This mirrors the layering convention in
  RFC-0009 and the Python harness's `*_adapter.py` suffix. The harness's
  architecture fitness test rejects direct imports of the common filesystem,
  process, network, environment, and HTTP modules from pure modules.
- **Only wiring imports adapters.** Purity says what a module may *do*; this
  says what it may *know*. A pure module never `use`s an adapter module
  directly — it declares a trait for what it needs, takes it as a generic
  parameter or `Arc<dyn Trait>`, and `main.rs` (or a dedicated wiring module)
  constructs the adapter and injects it.
  The architecture fitness test enforces this for modules named `*_adapter`.

- **Detached tasks preserve tracing context.** `tokio::spawn` and equivalent
  runtime spawns must wrap the complete future in `.instrument(task_span)`.
  Otherwise the task's telemetry becomes an orphan trace root. The harness
  rejects uninstrumented runtime spawns.

## Judgment rules

These need judgment, so no sensor owns them.

- **Functional-first, minimal state.** Rust needs structs to hold required
  state (a `ToolRouter`, an `Arc<Mutex<_>>` resource, a client handle) — that
  is a language requirement, not a style choice. Don't reach for a struct to
  hold *behavior* that could be a free function; keep pure logic in plain
  functions that take their inputs as arguments and return typed contract
  values, and keep the struct thin: state plus the `#[tool_router]` impl
  block that closes over it.
- **Tool doc comments are prompt material.** The `///` doc comment above a
  `#[tool(description = "...")]` method — and the `description` string
  itself — become the advertised description that steers the LLM's tool
  selection and arguments. Write it as contract, not as a code comment;
  changing it changes behavior.
- **One tool, one coherent outcome; Rule of Three.** Each tool exposes one
  clear, user-visible capability to the model; it may coordinate multiple
  internal steps to deliver it. Don't extract an abstraction until the
  pattern appears a third time.
- **Tool-level vs. protocol-level errors.** Use `Ok(CallToolResult::error(...))`
  when the tool ran but failed in a way the caller should see (the client
  renders it). Use `Err(McpError)` only when the server cannot route or
  process the request at all (clients render these opaquely). See the rmcp
  README's "Error handling" section for the full rationale.
- **PR discipline.** Conventional Commits; the PR body explains the *why*;
  refactoring and behavior changes in separate commits. When an agent
  stumbles on missing context, fix `AGENTS.md` in the same PR.

## `#[allow(...)]` conventions

Clippy lint levels are judgment tripwires, not absolutes. When a suppression
is legitimately needed, state the reason on the same line:

```rust
#[allow(clippy::unwrap_used)] // startup-only: an invalid config here should panic before serving
```

The suppression stays visible and is a review entry point. A reasonless
`#[allow(...)]` is what review rejects.

## Behavior-test authoring recipe

Call pure logic directly, assert on returned contract types. Never build a
`Greeter` and drive it through the MCP transport to test logic — fake the
adapter behind a pure function instead.

<!-- PLACEHOLDER: replace the example and fixture table with this repo's
real fixtures once the first adapter lands. Until then the template's own
test is the pattern: -->

```rust
#[test]
fn greeting_addresses_the_caller_by_name() {
    let result = build_greeting("Ada"); // real logic, called directly — no server, no mocks
    assert_eq!(result, GreetingResult { greeting: "Hello, Ada!".to_string(), language: Language::En });
}
```

Fixture table:

| Fixture | Where | Fakes |
|---|---|---|
| `ScriptedHttp` | `tests/common/mod.rs` | `HttpFetcher` (`http_fetch_adapter`): a URL → (status, content type, body) table; unknown URLs time out |
| `ScriptedModel` | `tests/common/mod.rs` | `ChatModel` (`llm_adapter`): first rule whose needle the prompt contains answers |
| `ScriptedEngine`, `FixedEngine` | `tests/search.rs`, `tests/research.rs` | `SearchEngine` (`searxng_adapter`) |
| `ScriptedRenderer` | `tests/extract.rs` | `Renderer` (`render_adapter`) |

Pure modules that need address types import them from `core::net` (values
only, no I/O), not `std::net`, which the fitness test reserves for adapters.

The bridge starts a fresh child process for every sessionless tool call, so
in-process state (the search cache, the warm browser pool) only lives as
long as one session.
