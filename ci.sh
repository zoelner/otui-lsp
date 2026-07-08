#!/usr/bin/env bash
# Single quality gate for otui-lsp: the one place build/test checks live. Don't duplicate its
# logic elsewhere.
#
# Usage:
#   ./ci.sh            # full gate (fmt + clippy + tests) across the workspace
#   ./ci.sh --quick    # fmt + clippy only (skip tests) for fast local loops
#
# Auto-detects cargo-nextest (parallel tests) and falls back to `cargo test` if absent.
set -euo pipefail

cd "$(dirname "$0")"

# Make cargo available even in a fresh non-login shell (rustup default install).
if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi

QUICK=0
[ "${1:-}" = "--quick" ] && QUICK=1

echo "==> cargo fmt --all --check"
cargo fmt --all --check

echo "==> cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

if [ "$QUICK" -eq 1 ]; then
  echo "==> --quick: skipping tests"
  exit 0
fi

if command -v cargo-nextest >/dev/null 2>&1; then
  echo "==> cargo nextest run --workspace"
  cargo nextest run --workspace
else
  echo "==> cargo test --workspace  (install cargo-nextest for parallel runs)"
  cargo test --workspace
fi

echo "==> gate PASS"
