#!/usr/bin/env bash
#
# install-beeper.sh - wire Beeper into the Edison Watch MCP gateway on macOS.
#
# Reality check (2026-07): Beeper only exposes an MCP endpoint from the Beeper
# *Desktop app* (enabled once under Settings > Developers > MCP). The headless
# `beeper` Server ships no MCP endpoint (github.com/beeper/cli issue #20), so a
# fully unattended install is not possible today. This script automates the
# Edison side end to end and gives exact, checkable instructions for the few
# steps that Beeper and the dashboard still gate behind a human.
#
# What it automates:
#   1. Install prerequisites (node/npx, `edison-stdiod`).
#   2. Authorize this device to Edison Watch via the stdiod browser/device flow
#      (`edison-stdiod login`; no API key paste, no step-up dance).
#   3. Supervise the tunnel daemon (`edison-stdiod install`).
#   4. Submit Beeper's stdio MCP proxy (`npx @beeper/desktop-mcp`) as a tunnel
#      server (`edison-stdiod server add`, which requests admin approval).
#
# What still needs a human (each one printed with the exact action):
#   A. Enable MCP in Beeper Desktop (Settings > Developers > MCP) so :23373
#      answers, and link WhatsApp / Telegram / etc. in the Beeper app.
#   B. Approve the submitted `beeper` server once in the Edison dashboard.
#   C. Set BEEPER_ACCESS_TOKEN on that server in the dashboard so the child can
#      authenticate. The script discovers a reusable token for you to paste.
#
# End-to-end topology once those are done:
#   AI client --MCP--> Edison Gateway --WS tunnel--> edison-stdiod
#     --stdio--> npx @beeper/desktop-mcp --HTTP :23373--> Beeper Desktop
#     --> WhatsApp / Telegram / LinkedIn / ...
#
# Built to be driven by an agent or a human: every input is a flag or an
# UPPER_SNAKE env var, nothing blocks on a prompt unless you pass --interactive,
# and missing inputs fail fast with the exact command to fix them.

set -euo pipefail

# Keep edison-stdiod (anyhow) from spilling a Rust backtrace on expected
# failures; we translate its exit codes into actionable messages ourselves.
export RUST_BACKTRACE="${RUST_BACKTRACE:-0}"
export RUST_LIB_BACKTRACE="${RUST_LIB_BACKTRACE:-0}"

# Keep `brew install` from dumping its auto-update wall and env hints. Respected
# only by Homebrew; harmless elsewhere.
export HOMEBREW_NO_AUTO_UPDATE="${HOMEBREW_NO_AUTO_UPDATE:-1}"
export HOMEBREW_NO_ENV_HINTS="${HOMEBREW_NO_ENV_HINTS:-1}"

# ---------------------------------------------------------------------------
# Defaults (every one overridable by flag or environment variable)
# ---------------------------------------------------------------------------
EW_BACKEND="${EW_BACKEND:-https://dashboard.edison.watch}"
EW_BACKEND_SET=0                                   # 1 once --ew-backend is given on the CLI
EW_API_KEY="${EW_API_KEY:-}"                       # only for the mcp-url client snippet
BEEPER_ACCESS_TOKEN="${BEEPER_ACCESS_TOKEN:-}"     # skip token discovery if set
SERVER_NAME="${SERVER_NAME:-beeper}"               # tunnel server name / gateway prefix
# Display label for this script's own output only. It does NOT set the stdiod
# device record: `edison-stdiod login` issues the device identity server-side.
DEVICE_LABEL="${DEVICE_LABEL:-$(hostname -s 2>/dev/null || echo my-mac)}"
MCP_PKG="${MCP_PKG:-@beeper/desktop-mcp}"          # the stdio proxy npx package

DRY_RUN=0
ASSUME_YES=0
INTERACTIVE=0
JSON=0
INSTALL_DEPS=0
VERBOSE=0
NO_COLOR_FLAG=0
NO_OPEN=0            # pass through to `edison-stdiod login --no-open` for headless auth
RELOGIN=0           # force a fresh `edison-stdiod login` even if already authorized

PROG="$(basename "$0")"

# ---------------------------------------------------------------------------
# Colors (auto-off when stderr is not a TTY, when NO_COLOR is set, or with
# --no-color, so piped and agent output stays a clean, parseable stream)
# ---------------------------------------------------------------------------
C_RESET=; C_BOLD=; C_DIM=; C_RED=; C_GREEN=; C_YELLOW=; C_BLUE=; C_CYAN=; C_GREY=
init_colors() {
  if [ "$NO_COLOR_FLAG" -eq 1 ] || [ -n "${NO_COLOR:-}" ] || [ ! -t 2 ] || [ "${TERM:-}" = "dumb" ]; then
    return 0
  fi
  C_RESET=$'\033[0m'; C_BOLD=$'\033[1m'; C_DIM=$'\033[2m'
  C_RED=$'\033[31m'; C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'
  C_BLUE=$'\033[34m'; C_CYAN=$'\033[36m'; C_GREY=$'\033[90m'
}

# ---------------------------------------------------------------------------
# Output helpers (data to stdout, diagnostics + progress to stderr)
# ---------------------------------------------------------------------------
log()  { printf '%s\n' "$*" >&2; }
step() { printf '%s%s>>%s %s%s\n' "$C_BOLD" "$C_BLUE" "$C_RESET" "$C_BOLD" "$*$C_RESET" >&2; }
ok()   { printf '   %s+%s %s\n' "$C_GREEN" "$C_RESET" "$*" >&2; }
info() { printf '   %s-%s %s%s%s\n' "$C_GREY" "$C_RESET" "$C_DIM" "$*" "$C_RESET" >&2; }
warn() { printf '   %s!%s %s%s%s\n' "$C_YELLOW" "$C_RESET" "$C_YELLOW" "$*" "$C_RESET" >&2; }
todo() { printf '   %saction:%s %s\n' "$C_CYAN" "$C_RESET" "$*" >&2; }
vlog() { [ "$VERBOSE" -eq 1 ] && printf '   %sdebug: %s%s\n' "$C_GREY" "$*" "$C_RESET" >&2 || true; }
die()  {
  printf '%s%sx error:%s %s\n' "$C_BOLD" "$C_RED" "$C_RESET" "$1" >&2
  [ -n "${2:-}" ] && printf '     %sfix:%s %s\n' "$C_CYAN" "$C_RESET" "$2" >&2
  exit "${3:-1}"
}

# run CMD... - previews under --dry-run instead of executing.
run() {
  if [ "$DRY_RUN" -eq 1 ]; then printf '   %swould run:%s %s%s%s\n' "$C_CYAN" "$C_RESET" "$C_DIM" "$*" "$C_RESET" >&2; return 0; fi
  vlog "run: $*"
  "$@"
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command '$1' not found" "${2:-install $1 and retry}"
}

confirm() {
  local ans
  [ "$ASSUME_YES" -eq 1 ] && return 0
  [ "$INTERACTIVE" -eq 0 ] && die "refusing to run a confirming action non-interactively: $1" \
    "pass --yes to proceed, or --dry-run to preview"
  printf '%s [y/N] ' "$1" >&2; read -r ans; [ "$ans" = "y" ] || [ "$ans" = "Y" ]
}

# macOS is the supported target because Beeper's MCP endpoint lives in the macOS
# Desktop app. stdiod also carries a Linux (systemd --user) supervisor path, so
# we allow Linux with a warning and let `edison-stdiod install` report any gap.
require_supported_platform() {
  case "$(uname -s)" in
    Darwin) ;;
    Linux)  warn "Linux is experimental: the stdiod supervisor needs a systemd --user session, and Beeper's MCP is macOS-Desktop-only, so the child will have nothing to reach";;
    *)      die "unsupported platform: $(uname -s)" "macOS is supported; see stdiod/README.md";;
  esac
}

# ---------------------------------------------------------------------------
# Flag parsing (shared across subcommands; unknown flags fail fast)
# ---------------------------------------------------------------------------
# Guard a value-taking flag before dereferencing its value. Args: remaining
# count ($#), the flag, and the candidate value. Routes through die() (our error
# contract) instead of `set -u`'s raw "unbound variable" when the value is
# missing (`install --ew-backend`), and rejects a flag-looking value so a
# forgotten argument does not silently swallow the next flag
# (`install --ew-backend --no-open`).
needval() {
  local n="$1" flag="$2" val="${3:-}"
  { [ "$n" -ge 2 ] && [ "${val#-}" = "$val" ]; } \
    || die "flag '$flag' needs a value" "example: $flag <value>"
}

parse_flags() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --ew-backend)   needval $# "$1" "${2:-}"; EW_BACKEND="$2"; EW_BACKEND_SET=1; shift 2;;
      --ew-api-key)   needval $# "$1" "${2:-}"; EW_API_KEY="$2"; shift 2;;
      --beeper-token) needval $# "$1" "${2:-}"; BEEPER_ACCESS_TOKEN="$2"; shift 2;;
      --server-name)  needval $# "$1" "${2:-}"; SERVER_NAME="$2"; shift 2;;
      --device-label) needval $# "$1" "${2:-}"; DEVICE_LABEL="$2"; shift 2;;
      --no-open)      NO_OPEN=1; shift;;
      --relogin)      RELOGIN=1; shift;;
      --dry-run)      DRY_RUN=1; shift;;
      -y|--yes)       ASSUME_YES=1; shift;;
      --interactive)  INTERACTIVE=1; shift;;
      --install-deps) INSTALL_DEPS=1; shift;;
      --no-color)     NO_COLOR_FLAG=1; shift;;
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
#
# ensure_tool <cmd> <human-fix> <install-cmd...>
#   - already present            -> no-op
#   - --dry-run                  -> preview the install command, never fail
#   - no consent to auto-install -> fail fast with <human-fix>
#     (consent = --install-deps, or an --interactive session)
#   - consent given              -> confirm (auto-passed by --yes), run the
#     installer, then VALIDATE the command actually landed on PATH
ensure_tool() {
  local cmd="$1" fix="$2"; shift 2
  command -v "$cmd" >/dev/null 2>&1 && return 0

  if [ "$DRY_RUN" -eq 1 ]; then
    info "dep '$cmd' missing; would install via: $*"
    return 0
  fi
  if [ "$INSTALL_DEPS" -eq 0 ] && [ "$INTERACTIVE" -eq 0 ]; then
    die "'$cmd' is not installed" "$fix"
  fi
  confirm "'$cmd' is missing. Install it now via: $*" \
    || die "declined; '$cmd' not installed" "$fix"
  command -v "$1" >/dev/null 2>&1 || die "cannot auto-install '$cmd': '$1' not found" "$fix"
  step "installing '$cmd' via: $*"
  "$@" || die "auto-install of '$cmd' failed" "$fix"
  command -v "$cmd" >/dev/null 2>&1 || die "'$cmd' still not on PATH after install" "$fix"
  ok "installed '$cmd'"
}

ensure_deps() {
  step "Checking prerequisites"
  require_supported_platform
  local stdiod_src; stdiod_src="$(dirname "$0")/../crates/edison-stdiod"
  ensure_tool npx \
    "install Node (brew install node) or re-run with --install-deps" \
    brew install --quiet node
  ensure_tool edison-stdiod \
    "run: cargo install --path crates/edison-stdiod   (or re-run with --install-deps)" \
    cargo install --path "$stdiod_src"
  if [ "$DRY_RUN" -eq 1 ]; then
    info "deps: preview only (nothing was installed)"
  else
    ok "npx and edison-stdiod present"
  fi
}

# ---------------------------------------------------------------------------
# Step A: Beeper Desktop app + its MCP endpoint
# ---------------------------------------------------------------------------
# Echo the first reachable Beeper Desktop API base URL, or nothing (exit 1).
# The app answers on 127.0.0.1:23373 by default; it may also bind IPv6 ([::1])
# or scan 23373-23378. BEEPER_API_URL overrides the probe.
beeper_api_base() {
  local wk="/.well-known/oauth-authorization-server" code h p url
  if [ -n "${BEEPER_API_URL:-}" ]; then
    code="$(curl -s -m 3 -o /dev/null -w '%{http_code}' "${BEEPER_API_URL}${wk}" 2>/dev/null || true)"
    [ -n "$code" ] && [ "$code" != "000" ] && { printf '%s' "$BEEPER_API_URL"; return 0; }
  fi
  for h in 127.0.0.1 localhost "[::1]"; do
    for p in 23373 23374 23375 23376 23377 23378; do
      url="http://$h:$p"
      code="$(curl -s -m 2 -o /dev/null -w '%{http_code}' "${url}${wk}" 2>/dev/null || true)"
      [ -n "$code" ] && [ "$code" != "000" ] && { printf '%s' "$url"; return 0; }
    done
  done
  return 1
}

# Non-fatal: if Beeper Desktop is not answering we still wire the automatable
# Edison side and print the exact action, so `install` makes progress instead of
# stopping the operator at the first prerequisite.
ensure_beeper_desktop() {
  step "Beeper Desktop MCP endpoint"
  if [ "$DRY_RUN" -eq 1 ]; then
    info "would probe 127.0.0.1:23373-23378 for the Beeper Desktop API"
    return 0
  fi
  local base
  if base="$(beeper_api_base)"; then
    ok "Beeper Desktop API reachable at $base"
    return 0
  fi
  warn "the Beeper Desktop API is not answering on 23373-23378"
  todo "open the Beeper Desktop app, then Settings > Developers > MCP and enable it"
  todo "link the chats you want (WhatsApp / Telegram / ...) inside the Beeper app"
  info "the headless 'beeper' Server does not expose MCP (github.com/beeper/cli issue #20), so the Desktop app must be running"
  warn "continuing to wire the Edison side; the Beeper child stays idle until this is enabled"
}

# ---------------------------------------------------------------------------
# Step B: Beeper access token (for the stdio MCP proxy child)
# ---------------------------------------------------------------------------
mask_token() {
  local s="$1"
  [ "${#s}" -le 12 ] && { printf '<len %s>' "${#s}"; return; }
  printf '%s...%s (len %s)' "${s:0:6}" "${s: -4}" "${#s}"
}

# Resolve the Beeper OAuth userinfo endpoint from the well-known document, with
# a sensible default. Echoes the URL; empty on no reachable API.
beeper_userinfo_endpoint() {
  local base uinfo
  base="$(beeper_api_base)" || return 1
  uinfo="$(curl -s -m 4 "$base/.well-known/oauth-authorization-server" 2>/dev/null \
           | grep -oE '"userinfo_endpoint"[[:space:]]*:[[:space:]]*"[^"]+"' \
           | grep -oE 'https?://[^"]+' | head -1)"
  [ -z "$uinfo" ] && uinfo="$base/oauth/userinfo"
  printf '%s' "$uinfo"
}

# discover_beeper_token - find a Desktop API bearer the Beeper app already holds,
# so you can paste it into the dashboard without minting a fresh one by hand.
# Beeper has no headless mint (OAuth is authorization_code + PKCE). The reliable
# invariant is that Beeper stores the bearer verbatim as "accessToken"; a capped
# generic scrape and the Keychain are best-effort fallbacks. Candidates are
# validated against the userinfo endpoint, first one that returns 200 wins.
# Prints the working token to stdout; diagnostics to stderr.
discover_beeper_token() {
  local uinfo
  uinfo="$(beeper_userinfo_endpoint)" || { warn "the Beeper Desktop API is not reachable on 23373-23378"; return 1; }

  # Search the Beeper config dir(s). Use an array so paths with spaces (the
  # macOS "Application Support" norm, or a spaced $HOME) survive intact.
  local dirs=("$HOME/.beeper") d cp
  if command -v beeper >/dev/null 2>&1; then
    cp="$(beeper config path 2>/dev/null || true)"
    if [ -n "$cp" ]; then
      if [ -d "$cp" ]; then dirs=("$cp" "${dirs[@]}"); else dirs=("$(dirname "$cp")" "${dirs[@]}"); fi
    fi
  fi

  # Primary (targeted): target files store the bearer verbatim as "accessToken".
  # Remember the first as the canonical fallback if live validation is flaky.
  local explicit="" f t cands=""
  for d in "${dirs[@]}"; do
    [ -d "$d" ] || continue
    while IFS= read -r f; do
      [ -n "$f" ] || continue
      t="$(sed -n 's/.*"accessToken"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$f" 2>/dev/null | head -n1)"
      if [ -n "$t" ]; then cands="$t
$cands"; [ -z "$explicit" ] && explicit="$t"; fi
    done <<EOF2
$(find "$d" -type f -name '*.json' 2>/dev/null)
EOF2
  done

  # Secondary (best-effort, capped): token-shaped strings from small JSON files.
  for d in "${dirs[@]}"; do
    [ -d "$d" ] || continue
    cands="$cands
$(find "$d" -type f -name '*.json' -size -1M -exec cat {} + 2>/dev/null \
      | grep -oE '[A-Za-z0-9._-]{24,}' | sort -u | head -n 40)"
  done

  # Best-effort Keychain fallback.
  if command -v security >/dev/null 2>&1; then
    local svc kc
    for svc in beeper Beeper beeper-cli com.beeper.cli "Beeper Desktop" "Beeper Desktop API"; do
      kc="$(security find-generic-password -s "$svc" -w 2>/dev/null || true)"
      [ -n "$kc" ] && cands="$cands
$kc"
    done
  fi

  # Validate candidates (deduped, order preserved so accessToken hits first).
  # Bounded so a bad guess set cannot turn this into minutes of silent curls.
  local tok code tried=0 max=20
  while IFS= read -r tok; do
    [ -z "$tok" ] && continue
    tried=$((tried + 1))
    if [ "$tried" -gt "$max" ]; then warn "checked $max candidate tokens without a live match; stopping"; break; fi
    code="$(curl -s -o /dev/null -w '%{http_code}' -m 3 -H "Authorization: Bearer $tok" "$uinfo" 2>/dev/null || true)"
    if [ "$code" = "200" ]; then printf '%s' "$tok"; return 0; fi
  done <<EOF
$(printf '%s\n' "$cands" | awk 'NF && !seen[$0]++')
EOF

  if [ -n "$explicit" ]; then
    warn "using the Beeper accessToken without a live userinfo check"
    printf '%s' "$explicit"; return 0
  fi
  return 1
}

# ---------------------------------------------------------------------------
# Shared: is SERVER_NAME already an approved server bound to this device?
# ---------------------------------------------------------------------------
# Single definition of the script's idempotency contract, used by install and
# doctor. Prefers a precise jq match on the server name; falls back to a raw
# substring grep when jq is absent.
server_registered() {
  command -v edison-stdiod >/dev/null 2>&1 || return 1
  local json; json="$(edison-stdiod server list --json 2>/dev/null || true)"
  [ -n "$json" ] || return 1
  if command -v jq >/dev/null 2>&1; then
    printf '%s' "$json" | jq -e --arg n "$SERVER_NAME" 'any(.. | objects; .name? == $n)' >/dev/null 2>&1
  else
    printf '%s' "$json" | grep -q "\"$SERVER_NAME\""
  fi
}

# ---------------------------------------------------------------------------
# Step 2 + 3: authorize this device and supervise the daemon
# ---------------------------------------------------------------------------
stdiod_config() { printf '%s' "${EDISON_STDIOD_CONFIG:-$HOME/.config/edison-stdiod/config.toml}"; }

# True when config already holds a client credential from a prior browser login.
stdiod_logged_in() {
  local f; f="$(stdiod_config)"
  [ -f "$f" ] && grep -q 'client_access_token' "$f" 2>/dev/null
}

# Echo the backend_url persisted in config (trailing slash stripped), or nothing.
stdiod_saved_backend() {
  local f; f="$(stdiod_config)"
  [ -f "$f" ] || return 0
  sed -n 's/^[[:space:]]*backend_url[[:space:]]*=[[:space:]]*"\{0,1\}\([^"]*\)"\{0,1\}.*/\1/p' \
    "$f" 2>/dev/null | head -n1 | sed 's:/*$::'
}

ensure_stdiod_auth() {
  step "Edison device authorization (browser)"
  if [ "$DRY_RUN" -eq 1 ]; then
    run edison-stdiod login --backend "$EW_BACKEND"
    return 0
  fi
  if [ "$RELOGIN" -eq 0 ] && stdiod_logged_in; then
    local saved; saved="$(stdiod_saved_backend)"
    if [ -n "$saved" ] && [ "$saved" != "${EW_BACKEND%/}" ]; then
      # An explicit --ew-backend that disagrees with the saved session is
      # ambiguous, so stop rather than silently target the wrong backend. With
      # no explicit flag, prefer the authorized session.
      if [ "$EW_BACKEND_SET" -eq 1 ]; then
        die "this device is authorized to ${saved}, but --ew-backend asked for ${EW_BACKEND}" \
          "pass --relogin to switch to ${EW_BACKEND}, or drop --ew-backend to keep ${saved}"
      fi
      warn "using the authorized backend ${saved} (pass --ew-backend <url> --relogin to switch)"
      EW_BACKEND="$saved"
    fi
    ok "already authorized on this device (client credential in $(stdiod_config))"
    return 0
  fi
  # `edison-stdiod login` runs the OAuth device flow: it prints a URL to approve
  # (and opens a browser unless --no-open), then stores a scoped client
  # credential. No API key, no step-up token.
  local args=(login --backend "$EW_BACKEND")
  [ "$NO_OPEN" -eq 1 ] && args+=(--no-open)
  info "a browser opens to approve this device; on a headless box pass --no-open and open the printed URL elsewhere"
  if ! run edison-stdiod "${args[@]}"; then
    die "edison-stdiod login failed" "check --ew-backend (${EW_BACKEND}) and complete the browser approval, then re-run: $PROG install"
  fi
  ok "device authorized to ${EW_BACKEND}"
}

ensure_stdiod_supervised() {
  step "Edison tunnel daemon (supervisor)"
  if ! run edison-stdiod install; then
    die "edison-stdiod install could not register the supervisor unit" \
      "macOS needs no privileges; Linux needs a logged-in systemd --user session. Fix that, then re-run: $PROG install"
  fi
  ok "daemon installed and supervised"
}

# ---------------------------------------------------------------------------
# Step 4: submit the Beeper stdio server for approval
# ---------------------------------------------------------------------------
# Under device authorization, `server add` submits a request scoped to this
# exact device (POST /api/v1/client/mcp-requests). An org admin approves it once
# in the dashboard; it does not run until then. `server_registered` lists only
# approved servers bound to this device, so we use it as the idempotency check.
submit_beeper_server() {
  step "Submitting the Beeper server"
  if [ "$DRY_RUN" -eq 1 ]; then
    run edison-stdiod server add "$SERVER_NAME" --display-name "Beeper" \
      --command npx --arg=-y --arg="$MCP_PKG"
    return 0
  fi
  if server_registered; then
    ok "server '$SERVER_NAME' is already approved and bound to this device"
    return 0
  fi
  # Capture with '&& rc=0 || rc=$?' so a non-zero add does not trip set -e.
  # Use --arg=VALUE: clap rejects a hyphen-leading value in the space form.
  local out rc
  out="$(edison-stdiod server add "$SERVER_NAME" --display-name "Beeper" \
        --command npx --arg=-y --arg="$MCP_PKG" 2>&1)" && rc=0 || rc=$?
  printf '%s\n' "$out" | grep -viE '^[[:space:]]*$' >&2 || true
  if [ "$rc" -ne 0 ]; then
    if printf '%s' "$out" | grep -qiE 'already (exists|submitted|pending)|duplicate'; then
      ok "a request for '$SERVER_NAME' is already pending; approve it in the dashboard"
      return 0
    fi
    die "edison-stdiod server add failed for '$SERVER_NAME'" \
      "check 'edison-stdiod status' shows the daemon connected, then re-run: $PROG install"
  fi
  ok "submitted '$SERVER_NAME' (npx $MCP_PKG) for approval"
  todo "approve '$SERVER_NAME' in the Edison dashboard: ${EW_BACKEND%/}  ->  Servers / requests"
}

# ---------------------------------------------------------------------------
# Step C: hand the Beeper token to the operator for the dashboard
# ---------------------------------------------------------------------------
# The device-scoped client request carries no env, and the daemon receives each
# server's environment from the backend (a ServerEnvUpdate frame merged into the
# device's per-installation env_store). So BEEPER_ACCESS_TOKEN is set in the
# dashboard when the server is configured, not by a call from this script. We
# discover a reusable token and print it (masked) with the exact place to paste.
report_beeper_token() {
  step "Beeper access token for the dashboard"
  local tok=""
  if [ -n "$BEEPER_ACCESS_TOKEN" ]; then
    tok="$BEEPER_ACCESS_TOKEN"
    ok "using the supplied token [$(mask_token "$tok")]"
  elif [ "$DRY_RUN" -eq 1 ]; then
    info "would discover a reusable Beeper token to paste into the dashboard"
    return 0
  elif tok="$(discover_beeper_token)" && [ -n "$tok" ]; then
    ok "discovered a working Beeper token [$(mask_token "$tok")]"
  else
    warn "could not discover a Beeper token automatically"
    todo "in Beeper Desktop: Settings > Developers > Beeper Desktop API > create/copy a token"
    tok=""
  fi
  todo "in the Edison dashboard, open server '$SERVER_NAME' and set env BEEPER_ACCESS_TOKEN=<token above>"
  info "the daemon respawns the child with that env once it is saved"
  # Keep the token off stdout so it is not captured by accident; print_result
  # only reports whether one was found.
  BEEPER_ACCESS_TOKEN="$tok"
}

# ---------------------------------------------------------------------------
# Result
# ---------------------------------------------------------------------------
print_result() {
  local mcp_url="${EW_BACKEND%/}/mcp"
  if [ "$JSON" -eq 1 ]; then
    printf '{"mcp_url":"%s","server":"%s","device_label":"%s","beeper_token_present":%s}\n' \
      "$mcp_url" "$SERVER_NAME" "$DEVICE_LABEL" "$([ -n "$BEEPER_ACCESS_TOKEN" ] && echo true || echo false)"
    return 0
  fi
  local b=$C_BOLD g=$C_GREEN d=$C_DIM r=$C_RESET
  [ -t 1 ] || { b=; g=; d=; r=; }
  printf '%smcp_url:%s %s%s%s\n' "$b" "$r" "$g" "$mcp_url" "$r"
  printf '%sserver:%s  %s (gateway prefix: %s_*)\n' "$b" "$r" "$SERVER_NAME" "$SERVER_NAME"
  printf '%sdevice:%s  %s (display label)\n' "$b" "$r" "$DEVICE_LABEL"
  if [ -n "$EW_API_KEY" ]; then
    printf '\n%s# add to Claude Code (gateway auth uses your Edison API key):%s\n' "$d" "$r"
    printf 'claude mcp add edison %s -t http -H "Authorization: Bearer %s" -s user\n' "$mcp_url" "$EW_API_KEY"
  else
    printf '\n%s# the AI client authenticates to the gateway with your Edison API key or OAuth;%s\n' "$d" "$r"
    printf '%s# pass --ew-api-key to print a ready-to-run claude-mcp-add snippet.%s\n' "$d" "$r"
  fi
}

# ===========================================================================
# Subcommands
# ===========================================================================
cmd_install() {
  ensure_deps
  ensure_beeper_desktop
  ensure_stdiod_auth
  ensure_stdiod_supervised
  submit_beeper_server
  report_beeper_token
  printf '\n%s%s== Edison side wired ==%s\n' "$C_BOLD" "$C_GREEN" "$C_RESET" >&2
  log "remaining human steps are printed above as 'action:' lines (approve the server, set the token)."
  print_result
}

cmd_doctor() {
  step "Doctor"
  local allgood=1
  for c in npx edison-stdiod; do
    if command -v "$c" >/dev/null 2>&1; then ok "$c"; else warn "$c missing"; allgood=0; fi
  done
  if beeper_api_base >/dev/null 2>&1; then ok "Beeper Desktop API reachable"; else warn "Beeper Desktop API not reachable (enable MCP in Beeper Desktop)"; allgood=0; fi
  if stdiod_logged_in; then ok "device authorized to Edison"; else warn "not authorized (run: $PROG install)"; allgood=0; fi
  if command -v edison-stdiod >/dev/null 2>&1 && edison-stdiod status >/dev/null 2>&1; then
    ok "stdiod daemon connected"; else warn "stdiod daemon not running (run: $PROG install)"; allgood=0; fi
  if server_registered; then
    ok "server '$SERVER_NAME' approved on this device"; else warn "server '$SERVER_NAME' not approved yet (submit + approve in dashboard)"; fi
  if [ "$allgood" -eq 1 ]; then ok "core checks passed"; else die "some checks failed (see above)" "$PROG install --install-deps"; fi
}

cmd_status() {
  need_cmd edison-stdiod
  run edison-stdiod status
  local base; base="$(beeper_api_base 2>/dev/null || true)"
  if [ -n "$base" ]; then ok "Beeper Desktop API: $base"; else warn "Beeper Desktop API not reachable"; fi
}

cmd_token() {
  step "Discovering a Beeper access token"
  if [ -n "$BEEPER_ACCESS_TOKEN" ]; then
    ok "a token is already supplied [$(mask_token "$BEEPER_ACCESS_TOKEN")]"
    return 0
  fi
  local tok
  if tok="$(discover_beeper_token)" && [ -n "$tok" ]; then
    ok "found a working token [$(mask_token "$tok")]"
    todo "set it as BEEPER_ACCESS_TOKEN on server '$SERVER_NAME' in the Edison dashboard"
    return 0
  fi
  warn "no reusable token found (is Beeper Desktop running with MCP enabled?)"
  die "no Beeper access token discovered" "create one in Beeper Desktop > Settings > Developers, or pass --beeper-token <TOKEN>"
}

cmd_mcp_url() { print_result; }

cmd_uninstall() {
  confirm "withdraw the '$SERVER_NAME' request/server and remove the stdiod supervisor unit?" || die "aborted" ""
  command -v edison-stdiod >/dev/null 2>&1 && {
    run edison-stdiod server remove "$SERVER_NAME" || true
    run edison-stdiod uninstall
  }
  log "uninstall complete. Approved-server removal may need a dashboard/admin action; Beeper Desktop was left untouched."
}

# ===========================================================================
# Help
# ===========================================================================
usage() {
  cat >&2 <<EOF
$PROG - wire Beeper into the Edison Watch MCP gateway (macOS)

Beeper only serves MCP from the Desktop app, so this automates the Edison side
and prints the exact human steps Beeper and the dashboard still require.

Usage:
  $PROG <command> [flags]

Commands:
  install     Deps, Beeper-Desktop check, device auth, supervise daemon, submit Beeper server
  doctor      Check prerequisites and current state (read-only)
  status      Show stdiod daemon + Beeper Desktop API status
  token       Discover a reusable Beeper token to paste into the dashboard
  mcp-url     Print the Edison MCP URL and client snippet
  uninstall   Withdraw the server and remove the supervisor unit

Common flags (also settable as UPPER_SNAKE env vars):
  --ew-backend URL     Edison backend        (EW_BACKEND, default $EW_BACKEND)
  --ew-api-key KEY     Edison API key for the client snippet only (EW_API_KEY)
  --beeper-token TOK   Beeper access token   (BEEPER_ACCESS_TOKEN) skips discovery
  --no-open            Headless device auth: print the approval URL, do not open a browser
  --relogin            Force a fresh device authorization even if already authorized
  --install-deps       Consent to auto-install missing deps (npx/edison-stdiod).
                       Confirms first unless --yes; validates each landed on PATH.
  --dry-run            Print what would run; change nothing
  --yes                Skip confirmations (agents pass this)
  --interactive        Allow interactive prompts as a fallback
  --json               Machine-readable output where supported
  --no-color           Disable colored output (also honors NO_COLOR)
  --verbose            Debug logging on stderr
  -h, --help           This help

Examples:
  # Agent-friendly: install deps and wire the Edison side, headless device auth
  $PROG install --install-deps --yes --no-open --ew-backend https://demo-dashboard.edison.watch

  # Preview without changing anything
  $PROG install --dry-run

  # Discover a Beeper token to paste into the dashboard
  $PROG token

Exit codes: 0 ok, 1 error (message + fix printed to stderr).
EOF
}

subcmd_help() {
  case "$1" in
    install)  log "install - wire the Edison side and print remaining human steps. Idempotent; safe to re-run."
              log "  optional: --ew-backend, --no-open, --install-deps, --yes, --dry-run.";;
    token)    log "token - discover a reusable Beeper token to paste into the dashboard.";;
    mcp-url)  log "mcp-url - print the gateway URL + client snippet. pass --ew-api-key for a ready-to-run snippet. supports --json.";;
    status)   log "status - show stdiod daemon + Beeper Desktop API status.";;
    doctor)   log "doctor - verify prerequisites and current state (read-only).";;
    uninstall)log "uninstall - withdraw the server and remove the supervisor unit. pass --yes to skip the prompt.";;
    *)        usage;;
  esac
}

# ===========================================================================
# Dispatch
# ===========================================================================
main() {
  local cmd="${1:-}"; shift || true
  ARGS=()
  parse_flags "$@" || { init_colors; subcmd_help "$cmd"; exit 0; }
  init_colors
  # No subcommand takes positional args; reject stray ones so typos are loud.
  [ "${#ARGS[@]}" -gt 0 ] && die "unexpected argument: ${ARGS[0]}" "run '$PROG --help' for usage"

  case "$cmd" in
    install)   cmd_install;;
    doctor)    cmd_doctor;;
    status)    cmd_status;;
    token)     cmd_token;;
    mcp-url)   cmd_mcp_url;;
    uninstall) cmd_uninstall;;
    ""|help|-h|--help) usage;;
    *) die "unknown command: $cmd" "run '$PROG --help' for the command list";;
  esac
}

main "$@"
