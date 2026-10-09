#!/usr/bin/env bash
# FreeChatCode rebuild: pull the latest and rebuild. No launch.
#
#   ./scripts/rebuild.sh
#
# Builds the current checkout into the installed `freechatcode` binary, then
# prints how to get started. It never launches a browser or a session.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> FreeChatCode rebuild"

git pull --ff-only 2>/dev/null || echo "  (not fast-forwarding; building the current checkout)"

echo "==> Rebuilding and reinstalling freechatcode…"
cargo install --path . --locked --bin freechatcode

echo
echo "==> Rebuilt. Get started:"
echo "  freechatcode --help           # commands and flags"
echo "  freechatcode doctor           # check each provider"
echo "  freechatcode launch opencode  # start a session"
