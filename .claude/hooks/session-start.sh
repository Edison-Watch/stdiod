#!/bin/bash
# Install deps + wire prek git hooks so cloud commits run the same checks as local.
# Scoped to remote (web/cloud) sessions; remove the guard to run locally too.
set -euo pipefail
[ "${CLAUDE_CODE_REMOTE:-}" != "true" ] && exit 0
cd "${CLAUDE_PROJECT_DIR:-.}"

export PATH="$HOME/.local/bin:$PATH"
line='export PATH="$HOME/.local/bin:$PATH"'
if [ -n "${CLAUDE_ENV_FILE:-}" ] && ! grep -qF "$line" "$CLAUDE_ENV_FILE" 2>/dev/null; then
  echo "$line" >> "$CLAUDE_ENV_FILE"
fi

# Install deps (Rust toolchain + fetch crates).
command -v cargo >/dev/null || curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y; cargo fetch

# Install prek (Rust binary, language-agnostic), then wire the git hooks.
command -v prek >/dev/null 2>&1 || curl -LsSf https://prek.j178.dev/install.sh | sh
prek install
exit 0
