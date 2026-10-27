# Rendered from tools-create-mcp-repo/rust-harness at 9a6d487.
# Do not edit directly; update the canonical harness and re-render.

# Deterministic harness — every sensor runs fully offline once dependencies
# are installed. `make harness` is what agents run before wrapping up; CI
# runs the same sensors as individual workflow steps.
#
# This file is byte-identical across every Rust MCP repo: no crate name and
# no source path appears in any command. `cargo fmt`/`cargo clippy`/`cargo
# test` all discover their targets from Cargo.toml.

HARNESS_DIR := .harness

.PHONY: harness
harness:
	@mkdir -p $(HARNESS_DIR); status=0; \
	run() { \
		if eval "$$2" >$(HARNESS_DIR)/$$1.log 2>&1; \
		then printf 'ok    %s\n' "$$1"; \
		else printf 'FAIL  %s (see $(HARNESS_DIR)/%s.log)\n' "$$1" "$$1"; status=1; fi; \
	}; \
	run fmt      "cargo fmt --all -- --check"; \
	run clippy   "cargo clippy --workspace --all-targets --all-features -- -D warnings"; \
	run test     "cargo test --workspace --all-targets --all-features"; \
	if command -v cargo-llvm-cov >/dev/null 2>&1; then \
		run coverage "cargo llvm-cov --workspace --all-features --summary-only"; \
	else \
		printf 'skip  coverage (cargo-llvm-cov not installed — see AGENTS.md; reported signal, never a gate)\n'; \
	fi; \
	exit $$status
