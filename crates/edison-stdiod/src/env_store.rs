//! Per-server environment variable store, kept on the device.
//!
//! Stdio MCP servers added via the dashboard pass their env values
//! (API keys, tokens) through the backend once and then live exclusively
//! here, on the user's machine. The backend persists only env var *names*
//! so a user can re-supply values later if a server fails to start.
//!
//! File layout (sibling to `config.toml`):
//!
//! ```text
//! ~/.config/edison-stdiod/
//!     config.toml          backend URL + credentials (mode 0600)
//!     server_envs.json     this file (mode 0600)
//! ```
//!
//! The store is a flat `{ server_id: { KEY: VALUE, ... }, ... }` JSON object.
//! Writes go through a temp file + rename so a crash mid-write never
//! corrupts the on-disk copy.

#![allow(dead_code)] // wired into the supervisor in a later commit

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

/// Map of `server_id -> { ENV_KEY: ENV_VALUE }`.
///
/// `BTreeMap` rather than `HashMap` so on-disk JSON is deterministic;
/// makes diffing the file in the wild far easier.
pub type EnvMap = BTreeMap<String, String>;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct OnDisk {
    #[serde(default)]
    servers: BTreeMap<String, EnvMap>,
}

/// In-memory view of the env store, with the disk path it was loaded
/// from. All mutation methods write through to disk before returning.
#[derive(Debug, Clone)]
pub struct EnvStore {
    path: PathBuf,
    data: BTreeMap<String, EnvMap>,
}

impl EnvStore {
    /// Open `~/.config/edison-stdiod/server_envs.json`. Missing file is
    /// fine and produces an empty store; first `set` will create it.
    pub fn open() -> Result<Self> {
        Self::open_at(paths::config_dir()?.join("server_envs.json"))
    }

    /// Testable variant that lets the caller pick the path.
    pub fn open_at(path: PathBuf) -> Result<Self> {
        let data = match std::fs::read_to_string(&path) {
            Ok(body) => {
                let parsed: OnDisk = serde_json::from_str(&body)
                    .with_context(|| format!("failed to parse {}", path.display()))?;
                parsed.servers
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                return Err(e).with_context(|| format!("failed to read {}", path.display()))
            }
        };
        Ok(Self { path, data })
    }

    /// Lookup env for a server, or `None` if the user never set any.
    pub fn get(&self, server_id: &str) -> Option<&EnvMap> {
        self.data.get(server_id)
    }

    /// Replace the env map for `server_id` and flush to disk. An empty
    /// `env` still inserts an entry (the user explicitly set zero vars);
    /// callers that want to *drop* the entry should use [`Self::remove`].
    pub fn set(&mut self, server_id: &str, env: EnvMap) -> Result<()> {
        self.data.insert(server_id.to_string(), env);
        self.flush()
    }

    /// Merge `env` into the existing entry for `server_id`, overwriting
    /// matching keys and keeping the rest, then flush. Creates the entry if
    /// the server has none yet.
    ///
    /// This is what the dashboard's "update one variable" path relies on:
    /// the backend forwards only the changed key(s) (it never holds the
    /// others), so a replace would silently drop every other variable. Merge
    /// keeps the untouched values intact. Removing a variable is a structural
    /// change handled elsewhere, not through this path.
    pub fn merge(&mut self, server_id: &str, env: EnvMap) -> Result<()> {
        let entry = self.data.entry(server_id.to_string()).or_default();
        entry.extend(env);
        self.flush()
    }

    /// Drop the entry for `server_id`. No-op if the server isn't present.
    pub fn remove(&mut self, server_id: &str) -> Result<()> {
        if self.data.remove(server_id).is_some() {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        let on_disk = OnDisk {
            servers: self.data.clone(),
        };
        let body = serde_json::to_string_pretty(&on_disk)
            .context("serialising server_envs.json")?;

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let tmp = tmp_path_for(&self.path);
        std::fs::write(&tmp, body)
            .with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&tmp)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&tmp, perms)?;
        }
        std::fs::rename(&tmp, &self.path).with_context(|| {
            format!("renaming {} -> {}", tmp.display(), self.path.display())
        })?;
        Ok(())
    }
}

/// Resolve the env map the supervisor should use for spawn.
///
/// The local store is authoritative when present: env values live on the
/// device, and the backend only carries them in `ServerEnvUpdate` frames
/// (which write to the store). For legacy / not-yet-set entries we fall
/// back to whatever env the `DesiredServer` itself carries.
pub fn resolve_env_for_spawn(stored: Option<&EnvMap>, fallback: &EnvMap) -> EnvMap {
    match stored {
        Some(map) => map.clone(),
        None => fallback.clone(),
    }
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "edison-stdiod-envstore-{}-{}",
            std::process::id(),
            n,
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn open_missing_file_returns_empty_store() {
        let dir = tempdir();
        let store = EnvStore::open_at(dir.join("server_envs.json")).unwrap();
        assert!(store.get("anything").is_none());
    }

    #[test]
    fn set_then_get_roundtrip() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path).unwrap();
        store
            .set("srv-1", sample_env(&[("A", "1"), ("B", "2")]))
            .unwrap();
        let got = store.get("srv-1").unwrap();
        assert_eq!(got.get("A").map(String::as_str), Some("1"));
        assert_eq!(got.get("B").map(String::as_str), Some("2"));
    }

    #[test]
    fn set_persists_across_reopen() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        {
            let mut store = EnvStore::open_at(path.clone()).unwrap();
            store.set("srv-1", sample_env(&[("TOKEN", "abc")])).unwrap();
        }
        let store = EnvStore::open_at(path).unwrap();
        assert_eq!(
            store.get("srv-1").and_then(|m| m.get("TOKEN")).map(String::as_str),
            Some("abc"),
        );
    }

    #[test]
    fn empty_env_still_inserts_entry() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path.clone()).unwrap();
        store.set("srv-1", EnvMap::new()).unwrap();
        let store = EnvStore::open_at(path).unwrap();
        assert!(store.get("srv-1").is_some());
        assert!(store.get("srv-1").unwrap().is_empty());
    }

    #[test]
    fn merge_keeps_untouched_keys_and_overwrites_matching() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path.clone()).unwrap();
        store
            .set("srv-1", sample_env(&[("A", "1"), ("B", "2")]))
            .unwrap();
        // Update only A and add C; B must survive.
        store
            .merge("srv-1", sample_env(&[("A", "updated"), ("C", "3")]))
            .unwrap();

        let store = EnvStore::open_at(path).unwrap();
        let got = store.get("srv-1").unwrap();
        assert_eq!(got.get("A").map(String::as_str), Some("updated"));
        assert_eq!(got.get("B").map(String::as_str), Some("2"));
        assert_eq!(got.get("C").map(String::as_str), Some("3"));
    }

    #[test]
    fn merge_creates_entry_when_absent() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path.clone()).unwrap();
        store.merge("brand-new", sample_env(&[("K", "v")])).unwrap();
        let store = EnvStore::open_at(path).unwrap();
        assert_eq!(
            store.get("brand-new").and_then(|m| m.get("K")).map(String::as_str),
            Some("v"),
        );
    }

    #[test]
    fn remove_drops_entry() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path.clone()).unwrap();
        store.set("srv-1", sample_env(&[("A", "1")])).unwrap();
        store.set("srv-2", sample_env(&[("B", "2")])).unwrap();
        store.remove("srv-1").unwrap();

        let store = EnvStore::open_at(path).unwrap();
        assert!(store.get("srv-1").is_none());
        assert!(store.get("srv-2").is_some());
    }

    #[test]
    fn remove_missing_is_noop() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path).unwrap();
        // Should not error; file may not even exist yet.
        store.remove("never-existed").unwrap();
    }

    #[test]
    fn multiple_servers_coexist() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path.clone()).unwrap();
        store.set("srv-1", sample_env(&[("A", "1")])).unwrap();
        store.set("srv-2", sample_env(&[("B", "2")])).unwrap();
        store.set("srv-3", sample_env(&[("C", "3")])).unwrap();

        let store = EnvStore::open_at(path).unwrap();
        assert_eq!(store.get("srv-1").unwrap().get("A").unwrap(), "1");
        assert_eq!(store.get("srv-2").unwrap().get("B").unwrap(), "2");
        assert_eq!(store.get("srv-3").unwrap().get("C").unwrap(), "3");
    }

    #[test]
    fn no_tmp_file_leftover_after_write() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path.clone()).unwrap();
        store.set("srv-1", sample_env(&[("A", "1")])).unwrap();
        let tmp = tmp_path_for(&path);
        assert!(!tmp.exists(), "tmp file should have been renamed away");
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn file_mode_is_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path.clone()).unwrap();
        store.set("srv-1", sample_env(&[("A", "1")])).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn resolve_env_uses_stored_when_present() {
        let stored = sample_env(&[("A", "stored")]);
        let fallback = sample_env(&[("A", "fallback"), ("B", "fallback")]);
        let resolved = resolve_env_for_spawn(Some(&stored), &fallback);
        assert_eq!(resolved.get("A").map(String::as_str), Some("stored"));
        assert!(resolved.get("B").is_none(), "stored map must win wholesale");
    }

    #[test]
    fn resolve_env_falls_back_when_no_stored_entry() {
        let fallback = sample_env(&[("A", "fallback")]);
        let resolved = resolve_env_for_spawn(None, &fallback);
        assert_eq!(resolved.get("A").map(String::as_str), Some("fallback"));
    }

    #[test]
    fn resolve_env_stored_empty_map_still_wins() {
        // User explicitly set "no env" on this server; we must not silently
        // re-introduce fallback values.
        let stored = EnvMap::new();
        let fallback = sample_env(&[("A", "fallback")]);
        let resolved = resolve_env_for_spawn(Some(&stored), &fallback);
        assert!(resolved.is_empty());
    }

    #[test]
    fn corrupt_file_surfaces_error() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        std::fs::write(&path, "{not json").unwrap();
        let err = EnvStore::open_at(path).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("failed to parse"), "got: {msg}");
    }
}
