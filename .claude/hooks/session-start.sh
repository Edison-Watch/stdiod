#!/bin/bash
# Install deps + wire prek git hooks so cloud commits run the same checks as local.
# Scoped to remote (web/cloud) sessions; remove the guard to run locally too.
set -euo pipefail
[ "${CLAUDE_CODE_REMOTE:-}" != "true" ] && exit 0
cd "${CLAUDE_PROJECT_DIR:-.}"

# rustup installs cargo under ~/.cargo/bin; prek installs under ~/.local/bin.
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
line='export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"'
if [ -n "${CLAUDE_ENV_FILE:-}" ] && ! grep -qF "$line" "$CLAUDE_ENV_FILE" 2>/dev/null; then
  echo "$line" >> "$CLAUDE_ENV_FILE"
fi

# Install deps (Rust toolchain + fetch crates). Source cargo env after a fresh
# rustup install so `cargo` is on PATH for this run.
if ! command -v cargo >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
fi
cargo fetch

# Install prek (Rust binary, language-agnostic), then wire the git hooks.
command -v prek >/dev/null 2>&1 || curl -LsSf https://prek.j178.dev/install.sh | sh
prek install
exit 0
