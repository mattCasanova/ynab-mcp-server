#!/usr/bin/env bash
# Launcher for Claude Code. Pulls the token from the macOS Keychain so it never
# lands in ~/.claude.json. One-time setup:
#   security add-generic-password -a "$USER" -s ynab-mcp -w '<personal access token>'
set -euo pipefail
YNAB_ACCESS_TOKEN="$(security find-generic-password -s ynab-mcp -w)"
export YNAB_ACCESS_TOKEN
exec "$(cd "$(dirname "$0")/.." && pwd)/target/release/ynab-mcp" "$@"
