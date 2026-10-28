# Contributing to mcp-websearch-server

Thanks for helping. This repository is public and licensed AGPL-3.0-only; a
change can merge once it is reviewed by a code owner, its checks pass, and its
author has signed the Contributor License Agreement below.

## Setup

- Rust stable with `rustfmt` and `clippy` (`rustup component add rustfmt clippy`).
- `cargo-llvm-cov` for the coverage report (`cargo install cargo-llvm-cov`).
- Docker, to build and smoke the image (`scripts/test-image-starts.sh`,
  `infra/run-image-smoke.sh`).
- Helm, for the chart checks (`scripts/test-networking-mode.sh`).
- Chrome or Chromium, only for the renderer tests that are ignored by default
  (`cargo test --lib -- --ignored`).

## Before you open a pull request

```bash
make harness
```

It runs every sensor CI runs (format, clippy with warnings denied, tests,
coverage) and prints one line per sensor; a failing sensor's full output is
in `.harness/<sensor>.log`. Fix the finding rather than suppressing it.
`AGENTS.md` lists the other commands and the rules the code follows (stdout is
the JSON-RPC wire, every tool returns `Json<T>`, only `*_adapter.rs` modules do
I/O).

## Pull requests

- Conventional Commits (`feat:`, `fix:`, `docs:`, `ci:` …); refactoring and
  behaviour changes in separate commits.
- The description explains why the change is needed.
- Behaviour changes come with a test of the pure logic (see the recipe in
  `AGENTS.md`).
- No secrets, tokens or internal hostnames in the diff.

## Licensing of contributions

This project is licensed under the GNU Affero General Public License v3.0 only
(`LICENSE`). By contributing, you agree that your contributions will be
licensed under the same license.

## Contributor License Agreement (CLA)

By contributing to this project, you agree that your contributions will be licensed under the same license as the project. Additionally, you affirm that:

1. You have the right to submit the contribution.
2. Your contribution does not violate any third-party rights.
3. You grant the project maintainers the right to use, modify, and distribute your contribution as part of the project.
4. You grant the project maintainers the right to relicense your contribution under additional licenses, including dual licensing or closed-source licenses, at their discretion, provided it remains consistent with the original license terms.

If you are contributing on behalf of an organization, you confirm that you have the necessary permissions to make the contribution on behalf of your organization.

### Signing

On your first pull request the CLA Assistant check asks you to sign. Reply on
the pull request with exactly:

```text
I have read the CLA Document and I hereby sign the CLA
```

The signature is recorded once and covers your later pull requests. Comment
`recheck` to run the check again.
