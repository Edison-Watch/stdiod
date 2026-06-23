//! Daemon supervisor: connects to the backend, reconciles desired state,
//! routes MCP frames between the WS and child subprocesses.
//!
//! Connection lifecycle:
//!
//! 1. Outer ``run`` loop keeps a long-lived [`Supervisor`] across WS
//!    reconnects. Children survive transient disconnects - their
//!    [`OutgoingHandle`] is rewired on each new WS so MCP frames keep
//!    flowing without restarting subprocesses.
//! 2. Each inner attempt: open WS → wire ``OutgoingHandle`` → send
//!    ``client_hello`` → spawn a heartbeat task → drain incoming frames
//!    until the WS closes.
//! 3. On disconnect we clear the handle (so children drop frames silently
//!    until reconnect) and back off with exponential delay + jitter.
//! 4. Auth failures (401/403) are surfaced as a hard error - v1.1 will
//!    instead enter a ``needs_reauth`` state per ARCHITECTURE.md.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Result};
use clap::Args;
use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;
use tracing::{debug, info, warn};
use tunnel_protocol::{
    ClientHello, DesiredServer, DesiredStateUpdate, McpFrame, ServerHello, ServerSpawnResult,
    ServerSpecUpdate, TunnelError, TunnelFrame, PROTOCOL_VERSION,
};

use crate::config;
use crate::env_store::{resolve_env_for_spawn, substitute_templated_args, EnvStore};
use crate::proc::ChildServer;
use crate::state::{ConnectionState, ServerEntry, ServerStatus, State, StateWriter};
use crate::tunnel::{self, OutgoingHandle};

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Backend base URL (http://localhost:8000, https://dashboard.edison.watch, …).
    /// Falls back to `backend_url` in `~/.config/edison-stdiod/config.toml`.
    #[arg(long, env = "EDISON_BACKEND_URL")]
    pub backend: Option<String>,
    /// API key (Bearer token). Falls back to `api_key` in config.toml.
    #[arg(long, env = "EDISON_API_KEY")]
    pub api_key: Option<String>,
    /// Optional edison secret key (X-Edison-Secret-Key).
    #[arg(long, env = "EDISON_SECRET_KEY")]
    pub edison_secret_key: Option<String>,
    /// Device identifier (must match the row in the backend's `devices` table).
    /// Defaults to the persisted `device_id`, then the machine hostname.
    #[arg(long, env = "EDISON_DEVICE_ID")]
    pub device_id: Option<String>,
    /// Human-readable device label (shown in the admin UI).
    #[arg(long, env = "EDISON_DEVICE_LABEL")]
    pub label: Option<String>,
}

/// Snapshot of the resolved values the rest of `daemon` cares about. Built
/// once per `run` from CLI flags overlaid on `~/.config/edison-stdiod/config.toml`.
struct ResolvedRun {
    backend: String,
    api_key: String,
    edison_secret_key: Option<String>,
    device_id: String,
    label: String,
}

impl ResolvedRun {
    fn from_args(args: RunArgs) -> Result<Self> {
        let persisted = config::PersistedConfig::load()?;
        let merged = config::Resolved::merge(
            persisted,
            config::Resolved {
                backend_url: args.backend,
                api_key: args.api_key,
                edison_secret_key: args.edison_secret_key,
                device_id: args.device_id,
                device_label: args.label,
            },
        );
        Ok(Self {
            backend: merged.backend_url()?.to_string(),
            api_key: merged.api_key()?.to_string(),
            edison_secret_key: merged.edison_secret_key.clone(),
            device_id: merged.device_id()?,
            label: merged.device_label(),
        })
    }
}

// Heartbeat tuning (per ARCHITECTURE.md "Disconnect handling"). The
// daemon pings every 15s and considers the connection dead if no pong
// arrives within HEARTBEAT_STALE_AFTER. On stale, the WS is closed and
// the outer reconnect loop kicks in.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const HEARTBEAT_STALE_AFTER: Duration = Duration::from_secs(25);

// If the wall-clock gap between heartbeat ticks far exceeds the interval, the
// machine slept/suspended: the socket is almost certainly dead and the monotonic
// clock paused during sleep (so HEARTBEAT_STALE_AFTER would be measured from wake).
// Tear down immediately on resume instead of waiting it out.
const HEARTBEAT_RESUME_GAP: Duration = Duration::from_secs(45);

// Exponential backoff with ±25% jitter, capped at 30s so a reconnect after the
// network returns (e.g. post-resume) isn't stranded behind a long backoff.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

pub async fn run(args: RunArgs) -> Result<()> {
    let resolved = ResolvedRun::from_args(args)?;
    // The supervisor - and the broker handle the children depend on -
    // live across reconnects. ``apply_snapshot`` on each new WS will
    // diff and reconcile.
    let outgoing = OutgoingHandle::new();

    // state.json is best-effort: ``StateWriter::update`` swallows write
    // failures so a full disk can never stall the WS loop. Seed it with
    // identity fields known at startup; per-transition fields (connection
    // state, last_error, servers[]) get rewritten by the lifecycle hooks
    // below.
    let writer = StateWriter::new(State {
        connection_state: ConnectionState::Starting,
        backend_url: Some(resolved.backend.clone()),
        device_id: Some(resolved.device_id.clone()),
        device_label: Some(resolved.label.clone()),
        ..State::default()
    });
    writer.update(|_| {}).await; // initial atomic write so `status` sees a file immediately

    let env_store = EnvStore::open()?;
    let supervisor = Arc::new(Mutex::new(Supervisor::new(
        outgoing.clone(),
        writer.clone(),
        env_store,
    )));

    let mut backoff = BACKOFF_MIN;
    loop {
        let result = run_one_session(&resolved, supervisor.clone(), &outgoing, &writer).await;
        match &result {
            Ok(()) => {
                info!("WS session ended cleanly; reconnecting");
                backoff = BACKOFF_MIN;
                writer
                    .update(|s| {
                        s.connection_state = ConnectionState::Reconnecting;
                        s.last_error = None;
                    })
                    .await;
            }
            Err(e) => {
                warn!(error = %e, "WS session ended with error; will retry");
                let msg = e.to_string();
                writer
                    .update(|s| {
                        s.connection_state = ConnectionState::Reconnecting;
                        s.last_error = Some(msg);
                    })
                    .await;
            }
        }
        outgoing.clear();
        let delay = jittered(backoff);
        info!(?delay, "sleeping before reconnect");
        sleep(delay).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// One connect + drain pass. Returns Ok when the WS closed cleanly (we'll
/// reconnect), Err on connect failure.
async fn run_one_session(
    args: &ResolvedRun,
    supervisor: Arc<Mutex<Supervisor>>,
    outgoing: &OutgoingHandle,
    writer: &StateWriter,
) -> Result<()> {
    let ws = tunnel::connect(
        &args.backend,
        &args.api_key,
        args.edison_secret_key.as_deref(),
        &args.device_id,
    )
    .await?;
    // WS upgrade succeeded - we've passed auth + the org feature flag.
    writer
        .update(|s| {
            s.connection_state = ConnectionState::Connected;
            s.last_connected_at = Some(chrono::Utc::now());
            s.last_error = None;
        })
        .await;

    let (outgoing_tx, outgoing_rx) = mpsc::channel::<TunnelFrame>(64);
    let (incoming_tx, mut incoming_rx) = mpsc::channel::<TunnelFrame>(64);

    // Wire the broker before any task can publish a frame.
    outgoing.set(outgoing_tx.clone());

    let ws_task = tokio::spawn(tunnel::run_frame_loop(ws, outgoing_rx, incoming_tx));

    // client_hello: announce which servers we already have running so the
    // backend can reconcile.
    let currently_running = supervisor
        .lock()
        .await
        .children
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    outgoing
        .send(TunnelFrame::ClientHello(ClientHello {
            protocol_version: PROTOCOL_VERSION,
            device_id: args.device_id.clone(),
            hostname: config::hostname(),
            label: args.label.clone(),
            os: config::current_os(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            currently_running,
        }))
        .await;

    // Heartbeat. ``last_pong`` is bumped by every inbound frame (Pong or
    // anything else - any traffic means the backend is alive).
    let last_pong = Arc::new(Mutex::new(Instant::now()));
    let mut hb_task = {
        let outgoing = outgoing.clone();
        let last_pong = last_pong.clone();
        tokio::spawn(heartbeat(outgoing, last_pong))
    };

    let result = tokio::select! {
        r = drain_incoming(supervisor, &mut incoming_rx, last_pong) => r,
        _ = &mut hb_task => {
            warn!("heartbeat: stale connection, tearing down session to reconnect");
            Ok(())
        }
    };
    hb_task.abort();
    ws_task.abort();
    drop(outgoing_tx);
    result
}

async fn drain_incoming(
    supervisor: Arc<Mutex<Supervisor>>,
    incoming_rx: &mut mpsc::Receiver<TunnelFrame>,
    last_pong: Arc<Mutex<Instant>>,
) -> Result<()> {
    while let Some(frame) = incoming_rx.recv().await {
        // Any inbound traffic counts as liveness, not just pongs.
        *last_pong.lock().await = Instant::now();

        let mut sup = supervisor.lock().await;
        match frame {
            TunnelFrame::ServerHello(ServerHello {
                protocol_version,
                servers,
            }) => {
                info!(
                    protocol_version,
                    server_count = servers.len(),
                    "received server_hello"
                );
                if protocol_version != PROTOCOL_VERSION {
                    warn!(
                        backend_version = protocol_version,
                        local_version = PROTOCOL_VERSION,
                        "protocol version mismatch; continuing in v1 MVP"
                    );
                }
                sup.apply_snapshot(servers).await;
            }
            TunnelFrame::DesiredStateUpdate(update) => sup.apply_delta(update).await,
            TunnelFrame::McpFrame(McpFrame { server_id, frame }) => {
                if let Some(child) = sup.children.get(&server_id) {
                    if let Err(e) = child.outbound_tx.send(frame).await {
                        warn!(server_id = %server_id, error = %e, "child outbound channel closed");
                    }
                } else {
                    warn!(server_id = %server_id, "mcp_frame for unknown server; dropping");
                }
            }
            TunnelFrame::TunnelError(err) => {
                warn!(?err, "tunnel_error from backend");
                // Device-wide (server_id=None) errors are soft rejections like
                // the ``stdio_tunnel_disabled`` org feature flag. Bail so the
                // outer reconnect loop persists the friendly message into
                // state.last_error - if we wrote it directly the loop's
                // Ok-branch (triggered by the backend's graceful close that
                // follows) would clear it again.
                if err.server_id.is_none() {
                    bail!("{}", err.message);
                }
            }
            TunnelFrame::Ping(_) => {
                drop(sup); // release before awaiting
                sup = supervisor.lock().await; // and re-acquire (no-op pattern keeps borrow checker happy)
                let _ = &sup;
                // (We just need to send a Pong. Use the broker so we don't
                // need to plumb the raw sender in here.)
                sup.tunnel_outgoing
                    .send(TunnelFrame::Pong(Default::default()))
                    .await;
            }
            TunnelFrame::ServerEnvUpdate(update) => {
                debug!(
                    server_id = %update.server_id,
                    env_keys = update.env.len(),
                    "applying server_env_update",
                );
                sup.apply_env_update(update.server_id, update.env).await;
            }
            TunnelFrame::ServerSpecUpdate(update) => {
                debug!(
                    server_id = %update.server_id,
                    env_keys = update.env.as_ref().map(|e| e.len()).unwrap_or(0),
                    templated_args = update.templated_args.as_ref().map(|t| t.len()).unwrap_or(0),
                    "applying server_spec_update",
                );
                sup.apply_spec_update(update).await;
            }
            TunnelFrame::ServerSpawnResult(_)
            | TunnelFrame::ClientHello(_)
            | TunnelFrame::Pong(_) => {
                // ServerSpawnResult is daemon→backend only; if it ever
                // arrives here it's a backend bug. ClientHello likewise
                // shouldn't come back from the backend. Pong is just
                // liveness, already bumped above.
            }
        }
    }
    Ok(())
}

async fn heartbeat(outgoing: OutgoingHandle, last_pong: Arc<Mutex<Instant>>) {
    let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
    // ``MissedTickBehavior::Delay`` so we don't burst pings after a pause.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_tick_wall = SystemTime::now();
    loop {
        tick.tick().await;

        // Detect a resume-from-sleep: a wall-clock jump much larger than the
        // interval means the process was frozen. Reconnect now rather than waiting
        // out the (monotonic) stale window, which only starts counting from wake.
        let now_wall = SystemTime::now();
        let wall_gap = now_wall.duration_since(last_tick_wall).unwrap_or_default();
        last_tick_wall = now_wall;
        if wall_gap > HEARTBEAT_RESUME_GAP {
            warn!(
                ?wall_gap,
                "heartbeat: large wall-clock gap (system resumed?), reconnecting"
            );
            outgoing.clear();
            return;
        }

        let stale = {
            let last = *last_pong.lock().await;
            Instant::now().saturating_duration_since(last)
        };
        if stale > HEARTBEAT_STALE_AFTER {
            warn!(
                ?stale,
                "heartbeat: no traffic from backend, closing connection"
            );
            // Dropping the only OutgoingHandle clone in our scope doesn't
            // close the underlying sender (the supervisor still holds one).
            // The cleanest way to force-close is to clear the broker so the
            // WS send task's channel drains and exits - but the WS task
            // also reads from the wire, and the read won't return until the
            // backend writes. We rely on the WS task's own keepalive and on
            // the supervisor-level reconnect; for v1.1 we'll add an
            // explicit ws-close signal here.
            outgoing.clear();
            return;
        }
        debug!("heartbeat: sending ping");
        outgoing.send(TunnelFrame::Ping(Default::default())).await;
    }
}

fn jittered(base: Duration) -> Duration {
    // ±25% jitter, computed without bringing in a full RNG crate. Uses the
    // process-monotonic nanos as a quick & dirty entropy source - fine for
    // backoff dispersion.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u64;
    let pct = (nanos % 51) as i64 - 25; // -25..=+25
    let base_ms = base.as_millis() as i64;
    let delta_ms = (base_ms * pct) / 100;
    let final_ms = (base_ms + delta_ms).max(0) as u64;
    Duration::from_millis(final_ms)
}

/// Reconciles desired-state announcements against running children.
struct Supervisor {
    children: HashMap<String, ChildServer>,
    tunnel_outgoing: OutgoingHandle,
    state: StateWriter,
    env_store: EnvStore,
}

impl Supervisor {
    fn new(tunnel_outgoing: OutgoingHandle, state: StateWriter, env_store: EnvStore) -> Self {
        Self {
            children: HashMap::new(),
            tunnel_outgoing,
            state,
            env_store,
        }
    }

    /// Apply the device's stored values to the incoming `DesiredServer`:
    /// env is overlaid via [`resolve_env_for_spawn`], and any
    /// `templated_args` substitutions are applied to each arg as a substring
    /// rewrite. `command` / `args` (structure) / `working_dir` always come
    /// from `DesiredServer` - the daemon never invents the spec.
    fn enrich(&self, mut desired: DesiredServer) -> DesiredServer {
        if let Some(spec) = self.env_store.get(&desired.server_id) {
            if !spec.templated_args.is_empty() {
                desired.args = substitute_templated_args(&desired.args, &spec.templated_args);
            }
        }
        desired.env = resolve_env_for_spawn(self.env_store.get(&desired.server_id), &desired.env);
        desired
    }

    /// Build the ``servers`` entries for state.json from the live child
    /// map. Called after every supervisor mutation so the file stays in
    /// lockstep with reality.
    fn snapshot_entries(&self) -> Vec<ServerEntry> {
        self.children
            .iter()
            .map(|(name, child)| ServerEntry {
                name: name.clone(),
                state: ServerStatus::Running,
                pid: child.child.id(),
            })
            .collect()
    }

    async fn publish_state(&self) {
        let entries = self.snapshot_entries();
        self.state.update(|s| s.servers = entries).await;
    }

    async fn apply_snapshot(&mut self, desired: Vec<DesiredServer>) {
        // Hold the *raw* (pre-enrich) DesiredServer here so respawn paths
        // (apply_spec_update / apply_env_update) can re-enrich against the
        // freshly-updated env_store. Enrichment runs inside try_spawn.
        let wanted: HashMap<String, DesiredServer> = desired
            .into_iter()
            .map(|d| (d.server_id.clone(), d))
            .collect();

        // Kill servers no longer in the snapshot (or now disabled).
        let to_drop: Vec<String> = self
            .children
            .keys()
            .filter(|id| wanted.get(*id).map(|d| !d.enabled).unwrap_or(true))
            .cloned()
            .collect();
        for id in to_drop {
            if let Some(child) = self.children.remove(&id) {
                child.shutdown().await;
            }
        }

        // Spawn newly-desired enabled servers, or respawn any whose spec
        // changed while we were disconnected. Identical-spec children are
        // left alone so reconnects don't gratuitously restart anything.
        // Equality is checked against the raw (backend-authoritative)
        // DesiredServer; env_store-only changes are not raw differences and
        // arrive separately as ServerSpecUpdate / ServerEnvUpdate, which
        // handle their own respawn.
        for (id, desired) in wanted {
            if !desired.enabled {
                continue;
            }
            if let Some(existing) = self.children.get(&id) {
                if existing.desired_raw == desired {
                    continue;
                }
                if let Some(stale) = self.children.remove(&id) {
                    stale.shutdown().await;
                }
            }
            self.try_spawn(desired).await;
        }
        self.publish_state().await;
    }

    /// Spawn a single desired server. Emits a ``ServerSpawnResult`` either
    /// way so the backend can gate its create_server HTTP response on the
    /// actual spawn outcome. On failure we also emit the legacy
    /// ``tunnel_error{spawn_failed}`` for any older backend that listens
    /// for it.
    ///
    /// Takes the *raw* DesiredServer (backend-authoritative, `{KEY}`
    /// placeholders intact). Enrichment runs here against the current
    /// env_store so subsequent respawns triggered by
    /// ``ServerSpecUpdate`` / ``ServerEnvUpdate`` always read the latest
    /// values - re-enriching an already-substituted spec would be a no-op.
    async fn try_spawn(&mut self, raw: DesiredServer) {
        let server_id = raw.server_id.clone();
        let enriched = self.enrich(raw.clone());
        match ChildServer::spawn(&raw, &enriched, self.tunnel_outgoing.clone()) {
            Ok(child) => {
                self.children.insert(server_id.clone(), child);
                self.tunnel_outgoing
                    .send(TunnelFrame::ServerSpawnResult(ServerSpawnResult {
                        server_id,
                        ok: true,
                        error: None,
                    }))
                    .await;
            }
            Err(e) => {
                let message = format!("failed to spawn `{}`: {e}", enriched.command);
                warn!(server_id = %server_id, error = %e, "spawn failed");
                self.tunnel_outgoing
                    .send(TunnelFrame::ServerSpawnResult(ServerSpawnResult {
                        server_id: server_id.clone(),
                        ok: false,
                        error: Some(message.clone()),
                    }))
                    .await;
                self.tunnel_outgoing
                    .send(TunnelFrame::TunnelError(TunnelError {
                        server_id: Some(server_id),
                        related_jsonrpc_id: None,
                        code: "spawn_failed".into(),
                        message,
                    }))
                    .await;
            }
        }
    }

    /// Persist a full resolved spec for a server (the backend's substituted
    /// command/args/env/working_dir) and respawn if it's currently running so
    /// the new spec takes effect immediately. No-op on not-yet-known
    /// servers; the spec is still saved for whenever the matching
    /// `DesiredStateUpdate` arrives. Wholesale replace (not merge) on the
    /// store side: each `ServerSpecUpdate` carries the complete resolved
    /// view, so previously-staged stale fields shouldn't linger.
    async fn apply_spec_update(&mut self, update: ServerSpecUpdate) {
        let server_id = update.server_id.clone();
        if let Err(e) =
            self.env_store
                .merge_template_values(&server_id, update.env, update.templated_args)
        {
            warn!(server_id = %server_id, error = %e, "failed to persist server_spec_update");
            return;
        }
        if let Some(existing) = self.children.remove(&server_id) {
            // Clone the *raw* spec so ``try_spawn``'s internal ``enrich``
            // can re-apply ``{KEY}`` substitution against the freshly-
            // merged env_store. Cloning ``existing.spec`` (the already-
            // substituted form) would leave ``substitute_templated_args``
            // nothing to find and the stale values would stay baked in.
            let raw = existing.desired_raw.clone();
            existing.shutdown().await;
            self.try_spawn(raw).await;
            self.publish_state().await;
        }
    }

    /// Persist new env values for a server and restart it if it's already
    /// running so the new env takes effect immediately. No-op on
    /// not-yet-known servers - the env is still saved for whenever the
    /// matching `DesiredStateUpdate` arrives.
    async fn apply_env_update(
        &mut self,
        server_id: String,
        env: std::collections::BTreeMap<String, String>,
    ) {
        // Merge, not replace: the backend forwards only the changed keys (it
        // never holds the others), so replacing would drop every variable the
        // update didn't mention.
        if let Err(e) = self.env_store.merge_env(&server_id, env) {
            warn!(server_id = %server_id, error = %e, "failed to persist server_env_update");
            return;
        }
        if let Some(existing) = self.children.remove(&server_id) {
            // Raw spec, same reasoning as apply_spec_update above:
            // ``try_spawn`` re-enriches against the updated env_store.
            let raw = existing.desired_raw.clone();
            existing.shutdown().await;
            self.try_spawn(raw).await;
            self.publish_state().await;
        }
    }

    async fn apply_delta(&mut self, delta: DesiredStateUpdate) {
        // ``added`` and ``updated`` are treated identically by spec, but
        // ``updated`` arrives often as a side effect of unrelated CRUD on
        // the same device (the backend resends the full current set as
        // ``updated`` whenever anything changes - see
        // ``push_desired_state`` in src/api/v1/routes/stdio_tunnel.py).
        // Killing+respawning a healthy child whose spec hasn't actually
        // changed silently invalidates the backend's already-initialized
        // MCP session against it: the new child sees the next ``tools/list``
        // as its first message and exits (the MCP lifecycle spec requires
        // ``initialize`` first).
        //
        // So: only restart when the spec genuinely differs from what we
        // last spawned with. ``enabled=false`` still tears the child down.
        // Iterate the raw DesiredServer here; ``try_spawn`` enriches
        // internally and stores the raw on ChildServer so subsequent
        // ServerSpecUpdate / ServerEnvUpdate respawns can re-enrich
        // against the latest env_store. Equality is on raw.
        for d in delta.added.into_iter().chain(delta.updated) {
            if !d.enabled {
                if let Some(existing) = self.children.remove(&d.server_id) {
                    existing.shutdown().await;
                }
                continue;
            }
            if let Some(existing) = self.children.get(&d.server_id) {
                if existing.desired_raw == d {
                    continue;
                }
            }
            if let Some(existing) = self.children.remove(&d.server_id) {
                existing.shutdown().await;
            }
            self.try_spawn(d).await;
        }
        for id in delta.removed {
            if let Some(child) = self.children.remove(&id) {
                child.shutdown().await;
            }
            if let Err(e) = self.env_store.remove(&id) {
                warn!(server_id = %id, error = %e, "failed to drop env_store entry on remove");
            }
        }
        self.publish_state().await;
    }
}
