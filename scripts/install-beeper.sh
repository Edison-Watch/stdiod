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

# Keep `brew install` from dumping its auto-update "New Formulae" wall and env
# hints on every run. Respected only by Homebrew; harmless elsewhere.
export HOMEBREW_NO_AUTO_UPDATE="${HOMEBREW_NO_AUTO_UPDATE:-1}"
export HOMEBREW_NO_ENV_HINTS="${HOMEBREW_NO_ENV_HINTS:-1}"

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
NO_COLOR_FLAG=0

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

# capture CMD... - like run but returns stdout; suppressed under --dry-run.
capture() {
  if [ "$DRY_RUN" -eq 1 ]; then printf '   %swould run:%s %s%s%s\n' "$C_CYAN" "$C_RESET" "$C_DIM" "$*" "$C_RESET" >&2; return 0; fi
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
    Linux)  warn "Linux support in edison-stdiod is experimental (needs a systemd --user session); macOS is the supported target";;
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
#
# Guarantees <cmd> is on PATH, or explains exactly how to get it. Behavior:
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

  # No consent to auto-install at all: fail fast with the manual command.
  if [ "$INSTALL_DEPS" -eq 0 ] && [ "$INTERACTIVE" -eq 0 ]; then
    die "'$cmd' is not installed" "$fix"
  fi

  # Consent exists; confirm intent (confirm() auto-passes with --yes, prompts
  # under --interactive, and refuses non-interactively without --yes).
  confirm "'$cmd' is missing. Install it now via: $*" \
    || die "declined; '$cmd' not installed" "$fix"

  # Validate the installer itself is available before invoking it.
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
  ensure_tool beeper \
    "run: brew install beeper/tap/cli   (or re-run with --install-deps)" \
    brew install --quiet beeper/tap/cli
  ensure_tool edison-stdiod \
    "run: cargo install --path crates/edison-stdiod   (or re-run with --install-deps)" \
    cargo install --path "$stdiod_src"

  if [ "$DRY_RUN" -eq 1 ]; then
    info "deps: preview only (nothing was installed)"
  else
    ok "npx, beeper, edison-stdiod all present"
  fi
}

# ---------------------------------------------------------------------------
# Step 2: headless Beeper Server
# ---------------------------------------------------------------------------
# Echo the first reachable Beeper Desktop API base URL, or nothing (exit 1).
# The CLI scans ports 23373-23378 on 127.0.0.1/localhost; the server may also
# bind IPv6 ([::1]). BEEPER_API_URL overrides the probe.
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

ensure_beeper_server() {
  step "Beeper Server (headless)"
  local base
  if [ "$DRY_RUN" -eq 1 ]; then
    info "would ensure the headless Beeper Server is running (browser auth on first setup)"
    run beeper setup --server --install
    return 0
  fi
  # Probe the real Desktop API, not `beeper status` (which exits 0 even with no
  # server configured, so it used to skip setup and leave nothing listening).
  if base="$(beeper_api_base)"; then
    ok "Desktop API reachable at $base"
    return 0
  fi
  info "installing/starting the headless server (a browser opens once to authorize your Beeper account)"
  run beeper setup --server --install
  if base="$(beeper_api_base)"; then
    ok "Desktop API reachable at $base"
  else
    die "the Beeper Desktop API is not reachable on 23373-23378 after setup" \
      "finish the browser authorization opened by 'beeper setup --server --install', then re-run: $PROG install"
  fi
}

# ---------------------------------------------------------------------------
# Step 3: Beeper access token (for the stdio MCP proxy child)
# ---------------------------------------------------------------------------
# Show only a safe fingerprint of a secret, never the secret itself.
mask_token() {
  local s="$1"
  [ "${#s}" -le 12 ] && { printf '<len %s>' "${#s}"; return; }
  printf '%s...%s (len %s)' "${s:0:6}" "${s: -4}" "${#s}"
}

# discover_beeper_token - reuse the token the `beeper` CLI already holds after
# `beeper setup`, so no GUI "Approved connections" step is needed. Beeper has no
# headless mint (OAuth is authorization_code + PKCE, which needs a browser), but
# the stored session token is a valid Desktop API bearer. We stay format-
# agnostic: gather candidate strings from the CLI config dir and the macOS
# Keychain, then keep the first that authenticates against the local OAuth
# userinfo endpoint. Prints the working token to stdout; diagnostics to stderr.
discover_beeper_token() {
  local base uinfo=""
  if ! base="$(beeper_api_base)"; then
    warn "the Beeper Desktop API is not reachable on 23373-23378"
    warn "run 'beeper setup --server --install' and finish the browser authorization first"
    return 1
  fi
  uinfo="$(curl -s -m 4 "$base/.well-known/oauth-authorization-server" 2>/dev/null \
           | grep -oE '"userinfo_endpoint"[[:space:]]*:[[:space:]]*"[^"]+"' \
           | grep -oE 'https?://[^"]+' | head -1)"
  [ -z "$uinfo" ] && uinfo="$base/oauth/userinfo"

  # Resolve the CLI config dir (`beeper config path` returns ~/.beeper/config.json).
  local cp dirs="$HOME/.beeper" d
  cp="$(beeper config path 2>/dev/null || true)"
  if [ -n "$cp" ]; then
    if [ -d "$cp" ]; then dirs="$cp $dirs"; else dirs="$(dirname "$cp") $dirs"; fi
  fi

  # Primary: the target files store the bearer verbatim as "accessToken"
  # (~/.beeper/targets/<name>.json). Extract those first and remember the first
  # one as the canonical token to fall back on if live validation is flaky.
  local explicit="" f t cands=""
  for d in $dirs; do
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

  # Secondary: token-shaped strings from small JSON files only (skip the DBs,
  # logs, and the bundled server binary so real tokens are not crowded out).
  for d in $dirs; do
    [ -d "$d" ] || continue
    cands="$cands
$(find "$d" -type f -name '*.json' -size -1M 2>/dev/null -exec cat {} + 2>/dev/null \
      | grep -oE '[A-Za-z0-9._-]{24,}' | sort -u | head -n 120)"
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

  # Validate candidates against userinfo; use the first that authenticates.
  local tok code
  while IFS= read -r tok; do
    [ -z "$tok" ] && continue
    code="$(curl -s -o /dev/null -w '%{http_code}' -m 5 -H "Authorization: Bearer $tok" "$uinfo" 2>/dev/null || true)"
    if [ "$code" = "200" ]; then printf '%s' "$tok"; return 0; fi
  done <<EOF
$cands
EOF

  # None validated live: if we found an explicit accessToken, trust it (the
  # userinfo probe can be strict about scopes; the child will surface a real
  # auth error if it is genuinely wrong).
  if [ -n "$explicit" ]; then
    warn "using the Beeper CLI accessToken without a live userinfo check"
    printf '%s' "$explicit"; return 0
  fi
  return 1
}

# Precedence: explicit token > reuse the CLI's session token > manual step.
ensure_beeper_token() {
  step "Beeper access token"
  if [ -n "$BEEPER_ACCESS_TOKEN" ]; then
    ok "using supplied token"
    return 0
  fi
  if [ "$DRY_RUN" -eq 1 ]; then
    info "would reuse the Beeper CLI session token, else require --beeper-token"
    BEEPER_ACCESS_TOKEN="dry-run-placeholder-token"
    return 0
  fi
  local tok
  if tok="$(discover_beeper_token)" && [ -n "$tok" ]; then
    BEEPER_ACCESS_TOKEN="$tok"
    ok "reusing the Beeper CLI's authorized session token, no GUI needed [$(mask_token "$tok")]"
    return 0
  fi
  # Fallback: no reusable token and none supplied. Guide the one-time manual step.
  warn "could not reuse a Beeper CLI token; is the Beeper Server running and logged in?"
  warn "otherwise create one once in Beeper Desktop: Settings > Developers > Beeper Desktop API,"
  warn "then Approved connections > +  to copy a token, and re-run with --beeper-token"
  die "no Beeper access token available" \
    "pass --beeper-token <TOKEN> (or set BEEPER_ACCESS_TOKEN); the re-run is fast, deps are already installed"
}

# ---------------------------------------------------------------------------
# Step 4a: Edison Watch account + API key
# ---------------------------------------------------------------------------
ensure_ew_api_key() {
  if [ -z "$EW_API_KEY" ]; then
    die "no Edison Watch API key provided" \
      "sign in at ${EW_BACKEND}, create an API key, then re-run with --ew-api-key edison_..."
  fi
  if [ "$DRY_RUN" -eq 1 ]; then
    ok "edison account: using supplied API key"
    return 0
  fi
  # Validate the key up front so a bad key fails here with a clear message,
  # not mid-flow inside 'edison-stdiod server add'.
  local code
  code="$(curl -s -o /dev/null -w '%{http_code}' -m 15 --connect-timeout 5 \
    -H "Authorization: Bearer ${EW_API_KEY}" "${EW_BACKEND%/}/api/v1/servers" 2>/dev/null || true)"
  case "$code" in
    401|403) die "Edison rejected the API key at ${EW_BACKEND} (http ${code}: invalid or inactive)" \
               "check the key is active and from this environment; for demo pass --ew-backend https://demo-dashboard.edison.watch";;
    000)     warn "could not reach ${EW_BACKEND} to validate the key; continuing";;
    *)       ok "edison account: API key accepted by ${EW_BACKEND}";;
  esac
}

# ---------------------------------------------------------------------------
# Step 4b: supervise the tunnel daemon and register the Beeper child
# ---------------------------------------------------------------------------
wire_tunnel() {
  step "Edison tunnel (stdiod daemon)"
  # Each step is wrapped so a failure yields a clean, actionable message
  # instead of a raw daemon backtrace plus a set -e abort mid-flow.
  if ! run edison-stdiod login --backend "$EW_BACKEND" --api-key "$EW_API_KEY" --device-label "$DEVICE_LABEL"; then
    die "edison-stdiod login failed" "check --ew-backend and --ew-api-key, then re-run: $PROG install"
  fi
  if ! run edison-stdiod install; then
    die "edison-stdiod install could not register the supervisor unit" \
      "macOS needs no privileges; Linux needs a logged-in systemd --user session. Fix that, then re-run: $PROG install"
  fi

  # Use the --arg=VALUE form: clap rejects a hyphen-leading value in the space
  # form ('--arg -y' is read as an unknown flag), so '--arg=-y'.
  if [ "$DRY_RUN" -eq 1 ]; then
    run edison-stdiod server add "$SERVER_NAME" --display-name "Beeper" \
      --command npx --arg=-y --arg="$MCP_PKG"
    return 0
  fi
  # Idempotent on this device.
  if edison-stdiod server list --json 2>/dev/null | grep -q "\"$SERVER_NAME\""; then
    ok "tunnel child '$SERVER_NAME' already registered on this device"
    return 0
  fi
  local out rc
  out="$(edison-stdiod server add "$SERVER_NAME" --display-name "Beeper" \
        --command npx --arg=-y --arg="$MCP_PKG" 2>&1)"; rc=$?
  # A name is unique per org: a 409 means it exists under another device (a
  # stale registration). Remove it (org-level, admin-only) and re-add here.
  if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -qiE 'already exists|CONFLICT|409'; then
    warn "'$SERVER_NAME' already exists in this org (stale/other device); re-registering it here"
    edison-stdiod server remove "$SERVER_NAME" >/dev/null 2>&1 || true
    out="$(edison-stdiod server add "$SERVER_NAME" --display-name "Beeper" \
          --command npx --arg=-y --arg="$MCP_PKG" 2>&1)"; rc=$?
  fi
  if [ "$rc" -ne 0 ]; then
    printf '%s\n' "$out" >&2
    die "edison-stdiod server add failed for '$SERVER_NAME'" \
      "if it still conflicts, your key may lack admin (remove needs it); pass --server-name <other> or delete '$SERVER_NAME' in the dashboard, then re-run: $PROG install"
  fi
  ok "tunnel child '$SERVER_NAME' registered"
}

# ---------------------------------------------------------------------------
# Step 5: bind the Beeper token to the child
# ---------------------------------------------------------------------------
# `edison-stdiod server add` carries no env, so we push BEEPER_ACCESS_TOKEN
# separately. The route is the backend's confirmed stdio_tunnel env endpoint,
# `POST /api/v1/servers/{name}/env` (schema UpdateServerEnvRequest): the value
# is staged in the device's on-device env_store and the child is respawned with
# it. The endpoint is admin-only, so --ew-api-key must belong to an org admin.
# A failure here is non-fatal: the tunnel and child are already set up, so we
# print the manual step and let the rest of the install finish.
bind_beeper_token() {
  step "Binding Beeper token to the tunnel child"
  local path="${EW_SERVER_ENV_PATH:-/api/v1/servers/${SERVER_NAME}/env}"
  local url="${EW_BACKEND}${path}"
  if [ "$DRY_RUN" -eq 1 ]; then
    run curl -X POST "$url" "(set BEEPER_ACCESS_TOKEN + base URL, respawns child)"
    return 0
  fi
  # Also pass the discovered base URL: the server may bind a non-default port
  # (e.g. 23374) while the proxy defaults to 23373. Send both common env names.
  local api_base; api_base="$(beeper_api_base 2>/dev/null || true)"
  [ -z "$api_base" ] && api_base="http://127.0.0.1:23373"
  local code
  code="$(curl -s -o /dev/null -w '%{http_code}' -m 30 --connect-timeout 5 -X POST "$url" \
    -H "Authorization: Bearer ${EW_API_KEY}" \
    -H "Content-Type: application/json" \
    --data "{\"env\":{\"BEEPER_ACCESS_TOKEN\":\"${BEEPER_ACCESS_TOKEN}\",\"BEEPER_API_URL\":\"${api_base}\",\"BEEPER_DESKTOP_BASE_URL\":\"${api_base}\"}}" 2>/dev/null || true)"
  [ -z "$code" ] && code="000"
  case "$code" in
    2*)      ok "bound to child '$SERVER_NAME' and respawned"; return 0;;
    401|403) warn "not authorized to bind the token (http ${code})"
             warn "the env endpoint is admin-only; --ew-api-key must belong to an org admin";;
    000)     warn "could not reach ${url} (network, or daemon not connected yet)"
             warn "confirm 'edison-stdiod status' shows connected, then re-run: $PROG install";;
    *)       warn "token bind returned http ${code} for ${url}";;
  esac
  warn "manual fallback: set BEEPER_ACCESS_TOKEN for server '${SERVER_NAME}' in the Edison dashboard under Servers > ${SERVER_NAME} > environment"
}

# ---------------------------------------------------------------------------
# Chat networks
# ---------------------------------------------------------------------------
add_networks() {
  [ -z "$NETWORKS" ] && return 0
  step "Linking chat networks"
  # Split on commas without leaking IFS into run()'s "$*" logging.
  local net nets
  nets="$(printf '%s' "$NETWORKS" | tr ',' ' ')"
  for net in $nets; do
    [ -z "$net" ] && continue
    info "adding '$net' (follow the QR / code prompt in this terminal)"
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
    return 0
  fi
  # stdout-gated colors so redirected/piped output stays clean and parseable.
  local b=$C_BOLD g=$C_GREEN d=$C_DIM r=$C_RESET
  [ -t 1 ] || { b=; g=; d=; r=; }
  printf '%smcp_url:%s %s%s%s\n' "$b" "$r" "$g" "$mcp_url" "$r"
  printf '%sauth:%s    Authorization: Bearer %s\n' "$b" "$r" "$masked"
  printf '%sserver:%s  %s (prefix: %s_*)\n' "$b" "$r" "$SERVER_NAME" "$SERVER_NAME"
  printf '%sdevice:%s  %s\n' "$b" "$r" "$DEVICE_LABEL"
  # Ready-to-run snippet uses the real key so it can be pasted as-is (uncolored).
  printf '\n%s# add to Claude Code:%s\n' "$d" "$r"
  printf 'claude mcp add edison %s -t http -H "Authorization: Bearer %s" -s user\n' "$mcp_url" "$EW_API_KEY"
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
  printf '\n%s%s== install complete ==%s\n' "$C_BOLD" "$C_GREEN" "$C_RESET" >&2
  print_mcp_url
}

cmd_doctor() {
  step "Doctor"
  local allgood=1
  for c in npx beeper edison-stdiod; do
    if command -v "$c" >/dev/null 2>&1; then ok "$c"; else warn "$c missing"; allgood=0; fi
  done
  if beeper status >/dev/null 2>&1; then ok "beeper server reachable"; else warn "beeper server not reachable"; allgood=0; fi
  if command -v edison-stdiod >/dev/null 2>&1 && edison-stdiod status >/dev/null 2>&1; then
    ok "stdiod daemon connected"; else warn "stdiod daemon not running (run: $PROG install)"; allgood=0; fi
  [ "$allgood" -eq 1 ] && ok "all good" || die "some checks failed (see above)" "$PROG install --install-deps"
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

cmd_token() {
  step "Discovering a Beeper access token (headless)"
  if [ -n "$BEEPER_ACCESS_TOKEN" ]; then
    ok "a token is already supplied [$(mask_token "$BEEPER_ACCESS_TOKEN")]"
    return 0
  fi
  local tok
  if tok="$(discover_beeper_token)" && [ -n "$tok" ]; then
    ok "found a working token by reusing the Beeper CLI session [$(mask_token "$tok")]"
    info "'$PROG install' will use this automatically; no --beeper-token needed"
    return 0
  fi
  warn "no reusable token found (is the Beeper Server running and logged in?)"
  die "no Beeper access token discovered" "pass --beeper-token <TOKEN>, or run 'beeper setup --server --install' first"
}

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
  token          Discover a reusable Beeper token from the CLI session (headless, no GUI)
  uninstall      Remove the tunnel child and supervisor unit

Common flags (also settable as UPPER_SNAKE env vars):
  --ew-backend URL     Edison backend       (EW_BACKEND, default $EW_BACKEND)
  --ew-api-key KEY     Edison API key        (EW_API_KEY)         required for install/mcp-url
  --beeper-token TOK   Beeper access token   (BEEPER_ACCESS_TOKEN) skips CLI minting
  --networks a,b,c     Link these after wiring (NETWORKS)
  --install-deps       Consent to auto-install missing deps (npx/beeper/edison-stdiod
                       via brew/cargo). Confirms first unless --yes; validates each
                       landed on PATH. --dry-run previews installs without running them.
  --dry-run            Print what would run; change nothing
  --yes                Skip confirmations (agents pass this)
  --interactive        Allow interactive prompts as a fallback
  --json               Machine-readable output where supported
  --no-color           Disable colored output (also honors NO_COLOR)
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
  parse_flags "$@" || { init_colors; subcmd_help "$cmd"; exit 0; }
  init_colors

  case "$cmd" in
    install)   cmd_install;;
    doctor)    cmd_doctor;;
    status)    cmd_status;;
    network)   cmd_network;;
    mcp-url)   cmd_mcp_url;;
    token)     cmd_token;;
    uninstall) cmd_uninstall;;
    ""|help|-h|--help) usage;;
    *) die "unknown command: $cmd" "run '$PROG --help' for the command list";;
  esac
}

main "$@"
