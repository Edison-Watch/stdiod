#!/usr/bin/env bash
# Build the edison-stdiod daemon as a fully STATIC Linux binary (x64 + arm64)
# and stage it under dist/. Static musl means zero glibc dependency - the same
# file runs on any distro (Debian, Fedora, Arch, Alpine, containers).
#
# Why cargo-zigbuild: rustls pulls in `ring`, whose C-crypto can't be
# cross-compiled to musl with the stock toolchain. zig (via cargo-zigbuild)
# supplies the C cross-toolchain. Works from macOS or Linux hosts. (On a Linux
# host you can also just `cargo build --release` for the host arch, with
# `musl-tools` installed, if you only need one arch natively.)
#
# Usage:  bash scripts/build-linux.sh                       # both arches
#         TARGET_ARCHES="x64" bash scripts/build-linux.sh   # one arch
#
# Prereqs:  brew install zig   (or see ziglang.org)
#           cargo install cargo-zigbuild
#           rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
OUT_DIR="$REPO_ROOT/dist"
BIN_NAME="edison-stdiod"

command -v zig >/dev/null 2>&1 || {
  echo "build-linux.sh: zig required (brew install zig / see ziglang.org)" >&2; exit 1; }
command -v cargo-zigbuild >/dev/null 2>&1 || {
  echo "build-linux.sh: cargo-zigbuild required (cargo install cargo-zigbuild)" >&2; exit 1; }

mkdir -p "$OUT_DIR"
WANT="${TARGET_ARCHES:-x64 arm64}"

for spec in "x64:x86_64-unknown-linux-musl" "arm64:aarch64-unknown-linux-musl"; do
  arch="${spec%%:*}"
  target="${spec##*:}"
  case " $WANT " in *" $arch "*) ;; *) continue ;; esac

  if ! rustup target list --installed | grep -q "^${target}\$"; then
    echo "Installing rustup target $target ..."
    rustup target add "$target"
  fi
  echo "Building $BIN_NAME for $target ..."
  ( cd "$REPO_ROOT" && cargo zigbuild --release --bin "$BIN_NAME" --target "$target" )
  cp "$REPO_ROOT/target/$target/release/$BIN_NAME" "$OUT_DIR/edison-stdiod-linux-$arch"
  chmod +x "$OUT_DIR/edison-stdiod-linux-$arch"
  echo "Staged -> $OUT_DIR/edison-stdiod-linux-$arch"
done

echo "Done. Static Linux binaries under $OUT_DIR/"
file "$OUT_DIR"/edison-stdiod-linux-* 2>/dev/null || true
