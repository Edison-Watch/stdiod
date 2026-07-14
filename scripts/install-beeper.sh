#!/usr/bin/env bash
#
# install-beeper.sh - one-command installer that wires Beeper into the Edison
# Watch MCP gateway on macOS.
#
# It does five things, each idempotent:
#   1. Install prerequisites (node/npx, the `beeper` CLI, `edison-stdiod`).
#   2. Bring up a headless Beeper Server on 127.0.0.1:23373 (one OAuth click).
#   3. Log the stdiod daemon in to an Edison Watch account and supervise it.
#   4. Register Beeper's stdio MCP proxy (`npx @beeper/desktop-mcp`) as a
#      tunnel child so the gateway can reach it.
#   5. Bind the Beeper access token so the child can authenticate, then print
#      the Edison MCP URL the user hands to their AI client.
#
# Design note: this is a thin orchestrator over two CLIs (`beeper` and
# `edison-stdiod`) plus a couple of REST calls. It is built to be driven by an
# agent OR a human: every input is a flag or an env var, missing required
# inputs fail fast with the exact command to fix them, and nothing blocks on an
# interactive prompt unless you opt in with `--interactive`.
#
# Verified command surfaces (2026-07):
#   beeper setup --server --install        headless Beeper Server
#   beeper accounts add <network>          link WhatsApp / Telegram / LinkedIn
#   beeper auth email start|response       non-browser email-code sign-in
#   edison-stdiod login|install|status     supervise the tunnel daemon
#   edison-stdiod server add|list|remove   register a stdio MCP child
#
# stdiod today supports the supervised daemon on macOS only. This script fails
# fast on other platforms and tells you so.

set -euo pipefail

# Keep edison-stdiod (anyhow) from spilling a Rust backtrace on expected
# failures; we translate its exit codes into actionable messages ourselves.
export RUST_BACKTRACE="${RUST_BACKTRACE:-0}"
export RUST_LIB_BACKTRACE="${RUST_LIB_BACKTRACE:-0}"

# ---------------------------------------------------------------------------
# Defaults (every one overridable by flag or environment variable)
# ---------------------------------------------------------------------------
EW_BACKEND="${EW_BACKEND:-https://dashboard.edison.watch}"
EW_API_KEY="${EW_API_KEY:-}"                       # skip account bootstrap if set
BEEPER_ACCESS_TOKEN="${BEEPER_ACCESS_TOKEN:-}"     # skip token minting if set
SERVER_NAME="${SERVER_NAME:-beeper}"               # tunnel child name / gateway prefix
DEVICE_LABEL="${DEVICE_LABEL:-$(hostname -s 2>/dev/null || echo my-mac)}"
NETWORKS="${NETWORKS:-}"                            # comma list: whatsapp,telegram,linkedin
MCP_PKG="${MCP_PKG:-@beeper/desktop-mcp}"          # the stdio proxy npx package

DRY_RUN=0
ASSUME_YES=0
INTERACTIVE=0
JSON=0
INSTALL_DEPS=0
VERBOSE=0

PROG="$(basename "$0")"

# ---------------------------------------------------------------------------
# Output helpers (data to stdout, diagnostics to stderr)
# ---------------------------------------------------------------------------
log()  { printf '%s\n' "$*" >&2; }
vlog() { [ "$VERBOSE" -eq 1 ] && printf 'debug: %s\n' "$*" >&2 || true; }
die()  { printf 'error: %s\n' "$1" >&2; [ -n "${2:-}" ] && printf '  fix: %s\n' "$2" >&2; exit "${3:-1}"; }

# run CMD... - echoes under --dry-run instead of executing.
run() {
  if [ "$DRY_RUN" -eq 1 ]; then printf 'would run: %s\n' "$*" >&2; return 0; fi
  vlog "run: $*"
  "$@"
}

# capture CMD... - like run but returns stdout; suppressed under --dry-run.
capture() {
  if [ "$DRY_RUN" -eq 1 ]; then printf 'would run: %s\n' "$*" >&2; return 0; fi
  "$@"
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command '$1' not found" "${2:-install $1 and retry}"
}

confirm() {
  [ "$ASSUME_YES" -eq 1 ] && return 0
  [ "$INTERACTIVE" -eq 0 ] && die "refusing to run a confirming action non-interactively: $1" \
    "pass --yes to proceed, or --dry-run to preview"
  printf '%s [y/N] ' "$1" >&2; read -r ans; [ "$ans" = "y" ] || [ "$ans" = "Y" ]
}

# macOS is the supported target. The binary also carries a Linux (systemd
# --user) path, so we allow Linux with a warning instead of blocking, and let
# `edison-stdiod install` report any capability gap itself. Windows is out.
require_supported_platform() {
  case "$(uname -s)" in
    Darwin) ;;
    Linux)  log "warning: Linux support in edison-stdiod is experimental (needs a systemd --user session); macOS is the supported target";;
    *)      die "unsupported platform: $(uname -s)" "macOS is supported; Linux is experimental; see stdiod/README.md";;
  esac
}

# ---------------------------------------------------------------------------
# Flag parsing (shared across subcommands; unknown flags fail fast)
# ---------------------------------------------------------------------------
parse_flags() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --ew-backend)   EW_BACKEND="$2"; shift 2;;
      --ew-api-key)   EW_API_KEY="$2"; shift 2;;
      --beeper-token) BEEPER_ACCESS_TOKEN="$2"; shift 2;;
      --server-name)  SERVER_NAME="$2"; shift 2;;
      --device-label) DEVICE_LABEL="$2"; shift 2;;
      --networks)     NETWORKS="$2"; shift 2;;
      --dry-run)      DRY_RUN=1; shift;;
      -y|--yes)       ASSUME_YES=1; shift;;
      --interactive)  INTERACTIVE=1; shift;;
      --install-deps) INSTALL_DEPS=1; shift;;
      --json)         JSON=1; shift;;
      --verbose)      VERBOSE=1; shift;;
      -h|--help)      return 10;;
      --) shift; break;;
      -*) die "unknown flag: $1" "run '$PROG <command> --help' for accepted flags";;
      *)  ARGS+=("$1"); shift;;
    esac
  done
}

# ---------------------------------------------------------------------------
# Step 1: prerequisites
# ---------------------------------------------------------------------------
ensure_deps() {
  require_supported_platform

  if ! command -v npx >/dev/null 2>&1; then
    if [ "$INSTALL_DEPS" -eq 1 ]; then
      need_cmd brew "install Homebrew from https://brew.sh, or install Node yourself"
      run brew install node
    else
      die "npx (Node.js) not found; $MCP_PKG runs via npx" \
        "install Node (brew install node) or re-run with --install-deps"
    fi
  fi

  if ! command -v beeper >/dev/null 2>&1; then
    if [ "$INSTALL_DEPS" -eq 1 ]; then
      need_cmd brew "install Homebrew from https://brew.sh"
      run brew install beeper/tap/cli
    else
      die "the 'beeper' CLI is not installed" \
        "run: brew install beeper/tap/cli   (or re-run this with --install-deps)"
    fi
  fi

  if ! command -v edison-stdiod >/dev/null 2>&1; then
    if [ "$INSTALL_DEPS" -eq 1 ]; then
      need_cmd cargo "install a Rust toolchain from https://rustup.rs"
      run cargo install --path "$(dirname "$0")/../crates/edison-stdiod"
    else
      die "the 'edison-stdiod' binary is not installed" \
        "run: cargo install --path crates/edison-stdiod   (or re-run with --install-deps)"
    fi
  fi
  log "deps ok: npx, beeper, edison-stdiod all present"
}

# ---------------------------------------------------------------------------
# Step 2: headless Beeper Server
# ---------------------------------------------------------------------------
ensure_beeper_server() {
  # `beeper status` exits 0 when a server target is adopted and reachable.
  if beeper status >/dev/null 2>&1; then
    log "beeper server: already running"
    return 0
  fi
  log "beeper server: installing headless server (a browser opens once to authorize your Beeper account)"
  run beeper setup --server --install
}

# ---------------------------------------------------------------------------
# Step 3: Beeper access token (for the stdio MCP proxy child)
# ---------------------------------------------------------------------------
# Precedence: explicit token > CLI-issued token > fail with the manual step.
ensure_beeper_token() {
  if [ -n "$BEEPER_ACCESS_TOKEN" ]; then
    log "beeper token: using supplied token"
    return 0
  fi
  if [ "$DRY_RUN" -eq 1 ]; then
    log "beeper token: would mint via CLI (or require --beeper-token)"
    BEEPER_ACCESS_TOKEN="dry-run-placeholder-token"
    return 0
  fi
  # The CLI can mint a Desktop API token for approved connections without a
  # browser once the server is authorized. If your CLI version exposes a
  # different verb, pass the token in via --beeper-token / BEEPER_ACCESS_TOKEN.
  local tok=""
  tok="$(capture beeper api post /v0/access-tokens --json 2>/dev/null \
        | sed -n 's/.*"token"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1 || true)"
  if [ -n "$tok" ]; then
    BEEPER_ACCESS_TOKEN="$tok"
    log "beeper token: minted via CLI"
    return 0
  fi
  die "could not obtain a Beeper access token automatically" \
    "create one in Beeper > Settings > Developers > Approved connections, then re-run with --beeper-token <TOKEN>"
}

# ---------------------------------------------------------------------------
# Step 4a: Edison Watch account + API key
# ---------------------------------------------------------------------------
ensure_ew_api_key() {
  if [ -n "$EW_API_KEY" ]; then
    log "edison account: using supplied API key"
    return 0
  fi
  die "no Edison Watch API key provided" \
    "sign in at ${EW_BACKEND}, create an API key, then re-run with --ew-api-key ew_live_..."
}

# ---------------------------------------------------------------------------
# Step 4b: supervise the tunnel daemon and register the Beeper child
# ---------------------------------------------------------------------------
wire_tunnel() {
  # Each step is wrapped so a failure yields a clean, actionable message
  # instead of a raw daemon backtrace plus a set -e abort mid-flow.
  if ! run edison-stdiod login --backend "$EW_BACKEND" --api-key "$EW_API_KEY" --device-label "$DEVICE_LABEL"; then
    die "edison-stdiod login failed" "check --ew-backend and --ew-api-key, then re-run: $PROG install"
  fi
  if ! run edison-stdiod install; then
    die "edison-stdiod install could not register the supervisor unit" \
      "macOS needs no privileges; Linux needs a logged-in systemd --user session. Fix that, then re-run: $PROG install"
  fi

  # Idempotent: only add the child if it is not already registered. The live
  # probe is skipped under --dry-run (nothing is registered to probe).
  if [ "$DRY_RUN" -eq 0 ] && edison-stdiod server list --json 2>/dev/null | grep -q "\"$SERVER_NAME\""; then
    log "tunnel child '$SERVER_NAME': already registered"
  else
    if ! run edison-stdiod server add "$SERVER_NAME" \
        --display-name "Beeper" \
        --command npx \
        --arg -y --arg "$MCP_PKG"; then
      die "edison-stdiod server add failed for '$SERVER_NAME'" \
        "confirm the daemon is logged in and the backend is reachable, then re-run: $PROG install"
    fi
    log "tunnel child '$SERVER_NAME': registered"
  fi
}

# ---------------------------------------------------------------------------
# Step 5: bind the Beeper token to the child
# ---------------------------------------------------------------------------
# `edison-stdiod server add` carries no env; the daemon receives per-child env
# from the backend (see stdiod env_store). We push BEEPER_ACCESS_TOKEN to the
# backend so it is stored as an Edison secret and injected at spawn. If the
# backend route is unavailable, we do not fail the whole install: the tunnel is
# up and the child is registered; only the token binding is pending, and we
# print the manual step.
bind_beeper_token() {
  local path="${EW_SERVER_ENV_PATH:-/api/v1/servers/${SERVER_NAME}/env}"
  local url="${EW_BACKEND}${path}"
  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'would run: curl -sf -X POST %s (set BEEPER_ACCESS_TOKEN)\n' "$url" >&2
    return 0
  fi
  local code
  code="$(curl -s -o /dev/null -w '%{http_code}' -m 15 --connect-timeout 5 -X POST "$url" \
    -H "Authorization: Bearer ${EW_API_KEY}" \
    -H "Content-Type: application/json" \
    --data "{\"env\":{\"BEEPER_ACCESS_TOKEN\":\"${BEEPER_ACCESS_TOKEN}\"}}" 2>/dev/null || true)"
  [ -z "$code" ] && code="000"
  case "$code" in
    2*) log "beeper token: bound to child '$SERVER_NAME' as an Edison secret";;
    *)  log "warning: could not bind the Beeper token via ${url} (http ${code})"
        log "  the tunnel and child are set up; bind the token manually in the Edison"
        log "  dashboard under Servers > ${SERVER_NAME} > environment, key BEEPER_ACCESS_TOKEN";;
  esac
}

# ---------------------------------------------------------------------------
# Chat networks
# ---------------------------------------------------------------------------
add_networks() {
  [ -z "$NETWORKS" ] && return 0
  # Split on commas without leaking IFS into run()'s "$*" logging.
  local net nets
  nets="$(printf '%s' "$NETWORKS" | tr ',' ' ')"
  for net in $nets; do
    [ -z "$net" ] && continue
    log "network: adding '$net' (follow the QR / code prompt in this terminal)"
    run beeper accounts add "$net"
  done
}

# ---------------------------------------------------------------------------
# Result
# ---------------------------------------------------------------------------
print_mcp_url() {
  local mcp_url="${EW_BACKEND%/}/mcp"
  local masked="${EW_API_KEY:0:10}..."
  if [ "$JSON" -eq 1 ]; then
    printf '{"mcp_url":"%s","auth_header":"Authorization: Bearer %s","server":"%s","device_label":"%s"}\n' \
      "$mcp_url" "$EW_API_KEY" "$SERVER_NAME" "$DEVICE_LABEL"
  else
    printf 'mcp_url: %s\n' "$mcp_url"
    printf 'auth:    Authorization: Bearer %s\n' "$masked"
    printf 'server:  %s (prefix: %s_*)\n' "$SERVER_NAME" "$SERVER_NAME"
    printf 'device:  %s\n' "$DEVICE_LABEL"
    # Ready-to-run snippet uses the real key so it can be pasted as-is.
    printf '\nclaude mcp add edison %s -t http -H "Authorization: Bearer %s" -s user\n' "$mcp_url" "$EW_API_KEY"
  fi
}

# ===========================================================================
# Subcommands
# ===========================================================================
cmd_install() {
  ensure_deps
  ensure_beeper_server
  ensure_beeper_token
  ensure_ew_api_key
  wire_tunnel
  bind_beeper_token
  add_networks
  log "install complete."
  print_mcp_url
}

cmd_doctor() {
  local ok=1
  for c in npx beeper edison-stdiod; do
    if command -v "$c" >/dev/null 2>&1; then log "ok   $c"; else log "MISS $c"; ok=0; fi
  done
  if beeper status >/dev/null 2>&1; then log "ok   beeper server reachable"; else log "MISS beeper server"; ok=0; fi
  if command -v edison-stdiod >/dev/null 2>&1 && edison-stdiod status >/dev/null 2>&1; then
    log "ok   stdiod daemon"; else log "MISS stdiod daemon (run: $PROG install)"; ok=0; fi
  [ "$ok" -eq 1 ] && log "doctor: all good" || die "doctor: some checks failed (see above)" "$PROG install --install-deps"
}

cmd_status() {
  need_cmd edison-stdiod
  run edison-stdiod status
  command -v beeper >/dev/null 2>&1 && run beeper status || true
}

cmd_network() {
  local verb="${ARGS[0]:-}"
  case "$verb" in
    add)  NETWORKS="${ARGS[1]:-}"; [ -z "$NETWORKS" ] && die "network name required" "$PROG network add whatsapp"; add_networks;;
    list) run beeper accounts list;;
    *)    die "unknown 'network' verb: ${verb:-<none>}" "$PROG network add whatsapp | $PROG network list";;
  esac
}

cmd_mcp_url() { ensure_ew_api_key; print_mcp_url; }

cmd_uninstall() {
  confirm "remove the stdiod supervisor unit and Beeper child?" || die "aborted" ""
  command -v edison-stdiod >/dev/null 2>&1 && {
    run edison-stdiod server remove "$SERVER_NAME" || true
    run edison-stdiod uninstall
  }
  log "uninstall complete. Beeper Server was left running; remove it with: beeper uninstall server"
}

# ===========================================================================
# Help
# ===========================================================================
usage() {
  cat >&2 <<EOF
$PROG - wire Beeper into the Edison Watch MCP gateway (macOS)

Usage:
  $PROG <command> [flags]

Commands:
  install        Full flow: deps, Beeper Server, tunnel, register + bind Beeper, print MCP URL
  doctor         Check prerequisites and current state (read-only)
  status         Show stdiod daemon and Beeper Server status
  network add    Link a chat network (whatsapp | telegram | linkedin | ...)
  network list   List linked chat networks
  mcp-url        Print the Edison MCP URL and client snippet
  uninstall      Remove the tunnel child and supervisor unit

Common flags (also settable as UPPER_SNAKE env vars):
  --ew-backend URL     Edison backend       (EW_BACKEND, default $EW_BACKEND)
  --ew-api-key KEY     Edison API key        (EW_API_KEY)         required for install/mcp-url
  --beeper-token TOK   Beeper access token   (BEEPER_ACCESS_TOKEN) skips CLI minting
  --networks a,b,c     Link these after wiring (NETWORKS)
  --install-deps       Auto-install npx/beeper/edison-stdiod via brew/cargo
  --dry-run            Print what would run; change nothing
  --yes                Skip confirmations (agents pass this)
  --interactive        Allow interactive prompts as a fallback
  --json               Machine-readable output where supported
  --verbose            Debug logging on stderr
  -h, --help           This help

Examples:
  # Non-interactive, agent-friendly: everything supplied up front
  $PROG install --install-deps --yes \\
    --ew-api-key ew_live_abc --beeper-token bpr_xyz \\
    --networks whatsapp,telegram,linkedin

  # Preview without changing anything
  $PROG install --ew-api-key ew_live_abc --dry-run

  # Link WhatsApp later, then fetch the URL as JSON for a config file
  $PROG network add whatsapp
  $PROG mcp-url --ew-api-key ew_live_abc --json

Exit codes: 0 ok, 1 error (message + fix printed to stderr).
EOF
}

subcmd_help() {
  case "$1" in
    install)  log "install - run the full wiring flow. Idempotent; safe to re-run."
              log "  needs: --ew-api-key (or EW_API_KEY). Optional: --beeper-token, --networks, --install-deps, --yes, --dry-run."
              log "  example: $PROG install --install-deps --yes --ew-api-key ew_live_abc --networks whatsapp";;
    network)  log "network add <name> | network list"
              log "  example: $PROG network add telegram";;
    mcp-url)  log "mcp-url - print the gateway URL + client snippet. needs --ew-api-key. supports --json.";;
    status)   log "status - show stdiod daemon + Beeper Server status.";;
    doctor)   log "doctor - verify prerequisites and current state (read-only).";;
    uninstall)log "uninstall - remove the tunnel child and supervisor unit. pass --yes to skip the prompt.";;
    *)        usage;;
  esac
}

# ===========================================================================
# Dispatch
# ===========================================================================
main() {
  local cmd="${1:-}"; shift || true
  ARGS=()
  # Support "network add" as a two-word command.
  if [ "$cmd" = "network" ]; then
    ARGS+=("${1:-}"); shift || true
  fi
  parse_flags "$@" || { subcmd_help "$cmd"; exit 0; }

  case "$cmd" in
    install)   cmd_install;;
    doctor)    cmd_doctor;;
    status)    cmd_status;;
    network)   cmd_network;;
    mcp-url)   cmd_mcp_url;;
    uninstall) cmd_uninstall;;
    ""|help|-h|--help) usage;;
    *) die "unknown command: $cmd" "run '$PROG --help' for the command list";;
  esac
}

main "$@"
