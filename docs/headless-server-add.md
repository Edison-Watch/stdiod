# Headless `server add` - the step-up blocker and how to fix it

## Problem

The install flow (`scripts/install-beeper.sh`, and any future one-command
onboarding) is fully headless **except** for one call: registering a
`stdio_tunnel` server via `POST /api/v1/servers`.

That endpoint gates `stdio_tunnel` creation behind a **step-up re-auth**:
the request must carry an `X-Edison-Step-Up-Token` - a Supabase JWT minted
by an *interactive* login no more than 5 minutes ago
(`_STEP_UP_MAX_AGE_SECONDS = 300`, in
`src/api/v1/routes/servers_crud_create.py`). A backend API key alone does
not satisfy it, so the CLI gets:

```
401 STEP_UP_REQUIRED: "Adding a local server requires a fresh re-login.
Re-authenticate in the dashboard and try again."
```

The CLI HTTP client only knows how to send `Authorization: Bearer <api_key>`
(`crates/edison-stdiod/src/http.rs`), and there is no headless way to obtain
a step-up JWT (it requires an interactive browser login). So the gate and
the "one command, no GUI" goal are in direct conflict.

```
  install script ──POST /api/v1/servers──▶ backend
        │  Bearer <api_key>                   │
        │  (no step-up token - can't mint     ▼
        │   one without a browser)     _require_recent_supabase_login
        │                              needs X-Edison-Step-Up-Token < 5 min
        └───────────────────────────▶ 401 STEP_UP_REQUIRED
```

Today's only escapes:
- `TEST_AUTH_MODE=local` (local dev only), or
- `STEP_UP_BYPASS_EMAIL_DOMAINS=<domain>` - honored on dev/demo, **ignored
  on release**.

Neither is a real production answer.

## Why the gate exists

`stdio_tunnel` servers run an **arbitrary command on the user's machine**
(`command` + `args`). An attacker who steals a long-lived API key could
otherwise register `command=/bin/sh` and get remote code execution through
the daemon. Requiring a fresh interactive login raises the bar from
"leaked key" to "leaked key + live human session". That protection is
worth keeping; the fix must preserve it, not delete it.

## The leverage point: the daemon is already a trusted device

The daemon authenticates to the backend over the WS tunnel and shows up in
the admin **Devices** page. The dangerous part of `server add` - "run this
command" - targets a *specific already-enrolled device* (`device_id` from
`config.toml`). So the trust we need isn't "is this a live browser
session"; it's "did a human, once, authorize *this device* to run local
servers".

That reframes the fix as **device enrollment**, not per-call re-auth.

## Proposal: device-pairing token

Mint a short-lived, single-purpose **pairing token** in the dashboard (one
interactive step-up, exactly where the human already is), hand it to the
daemon once at enrollment, and let the backend accept it in place of a
step-up JWT **only** for `stdio_tunnel` adds scoped to that device.

```
  ┌─ dashboard (human, one step-up) ─┐
  │  "Pair a device" → mint token    │
  │  pt_<opaque>, ttl≈15m, org+admin │
  └──────────────┬───────────────────┘
                 │  copy/paste OR deep link
                 ▼
  edison-stdiod login --pairing-token pt_<opaque>
                 │  POST /api/v1/devices/pair
                 │  { pairing_token, device_id, device_label }
                 ▼
  backend verifies pt_, binds device_id → org, returns a
  device credential stored in config.toml (0600)
                 │
                 ▼
  later: server add  ──Bearer api_key + X-Edison-Device-Cred──▶ backend
                 accepts device-cred in lieu of step-up *iff*
                 body.device_id == the paired device  → 200 OK
```

### Backend changes (`edison-watch`)
1. **Mint** - `POST /api/v1/devices/pairing-token`, admin + step-up
   required (reuses the existing interactive gate). Returns
   `pt_<opaque>`, TTL ~15 min, single use, scoped to `org_id`.
2. **Redeem** - `POST /api/v1/devices/pair` accepts `{pairing_token,
   device_id, device_label}`, verifies + burns the token, records the
   device as *authorized-for-local-servers*, and returns a device
   credential (or just marks the existing device row).
3. **Relax the gate, narrowly** - in `_require_recent_supabase_login`,
   accept a valid paired-device credential **only** when the request is a
   `stdio_tunnel` add whose `device_id` matches the paired device. Every
   other step-up call is unchanged.

### CLI changes (`stdiod`)
1. `login --pairing-token <pt>` → calls `/devices/pair`, stores the
   returned device credential next to `api_key` in `config.toml`.
2. `http.rs` attaches the device credential header on `server add` when
   present.
3. `install-beeper.sh` passes `--pairing-token` through (env or flag); if
   absent, it prints the dashboard "Pair a device" URL and stops with an
   actionable message instead of failing on a raw 401.

### Resulting UX
- **First device, ever:** one interactive step (mint the token in the
  dashboard) → paste into the installer → everything else headless.
- **Re-runs / re-installs on the same device:** fully headless (device
  already paired).
- **CI / fleet rollout:** mint one pairing token per device out-of-band,
  inject via env - no browser in the automation path.

This trades "interactive login on *every* add" for "interactive
enrollment *once per device*", which is both safer to reason about
(device is the unit of trust) and compatible with headless install.

## Alternatives considered

- **Loosen the gate to accept API keys** - rejected: reintroduces the
  leaked-key-→-RCE risk the gate exists to stop.
- **CLI mints its own step-up JWT** - not possible; step-up requires an
  interactive Supabase login by construction.
- **`STEP_UP_BYPASS_EMAIL_DOMAINS` in release** - rejected: domain-wide,
  static, and explicitly disabled in release for good reason.

## Interim (demo/testing only)

To exercise the full pipeline **today** without shipping the above, set
`STEP_UP_BYPASS_EMAIL_DOMAINS=<your-domain>` on the **demo** backend (it is
honored on non-release) and run the installer against
`--ew-backend https://demo-dashboard.edison.watch`. This is a test-only
unblock, not the production design.
