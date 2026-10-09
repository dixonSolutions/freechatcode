#!/usr/bin/env bash
# FreeChatCode setup: build & install the CLI, then hand off for setup guidance.
#
#   ./scripts/setup.sh                 # build, install, then run `freechatcode doctor`
#   ./scripts/setup.sh --provider gemini
#
# The doctor step opens each provider's browser and verifies the composer /
# sign-in, without typing anything. It is the "set up one provider at a time"
# path — sign in directly in the browser window if it opens.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> FreeChatCode setup"

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo (Rust) is required — install it from https://rustup.rs" >&2
    exit 1
fi

echo "==> Building and installing freechatcode (first build can take a few minutes)…"
cargo install --path . --locked --bin freechatcode

if ! command -v freechatcode >/dev/null 2>&1; then
    echo "warning: freechatcode was installed but is not on PATH." >&2
    echo "Add $(cargo --version >/dev/null 2>&1 && echo '$HOME/.cargo/bin') to your PATH, then run:" >&2
    echo "  freechatcode doctor" >&2
    exit 0
fi

echo
echo "==> Installed. Handing off to the CLI for setup guidance…"
exec freechatcode doctor "$@"
