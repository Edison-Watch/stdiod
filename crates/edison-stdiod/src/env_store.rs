//! Per-server *values* the device holds for stdio MCP servers.
//!
//! The daemon never reads the backend DB; the canonical command / args /
//! working_dir for each server come down on the wire in `DesiredServer`
//! (still with `{PP}`-style placeholders in args). This file only carries
//! the *values* the user has supplied on the dashboard:
//!
//! - `env`: env vars to add to the subprocess environment. Whether the
//!   admin defined the var with a template (`{API_TOKEN}`) or as a literal
//!   doesn't matter here - the device sees a flat `{KEY: VALUE}` map.
//! - `templated_args`: per-placeholder substitutions, keyed by the literal
//!   placeholder including braces (`"{PP}"`). At spawn the daemon replaces
//!   each occurrence of `"{PP}"` in `DesiredServer.args` with the supplied
//!   value, leaving the args structure otherwise untouched.
//!
//! File layout (sibling to `config.toml`):
//!
//! ```text
//! ~/.config/edison-stdiod/
//!     config.toml          backend URL + credentials (mode 0600)
//!     server_envs.json     this file (mode 0600)
//! ```
//!
//! ### On-disk shape
//!
//! ```json
//! {
//!   "servers": {
//!     "fs": {
//!       "env": { "M1": "MV1" },
//!       "templated_args": { "{PP}": "/Users/me" }
//!     }
//!   }
//! }
//! ```
//!
//! Writes go through a temp file + rename so a crash mid-write never
//! corrupts the on-disk copy.

#![allow(dead_code)] // wired into the supervisor in a later commit

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

/// Map of `ENV_KEY -> ENV_VALUE`. `BTreeMap` rather than `HashMap` so on-disk
/// JSON is deterministic; makes diffing the file in the wild far easier.
pub type EnvMap = BTreeMap<String, String>;

/// Per-server values stored on the device: env vars and per-placeholder
/// args substitutions. Both default to empty - a server with no template
/// variables and no env still has an entry the moment any value lands.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerSpec {
    #[serde(default)]
    pub env: EnvMap,
    /// Substring substitutions to apply to each arg at spawn time. Keys are
    /// the literal placeholder *with* braces (e.g. `"{PP}"`); values are
    /// what should replace them. The daemon does naive substring replace -
    /// no template-syntax parsing on this side.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub templated_args: BTreeMap<String, String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct OnDisk {
    #[serde(default)]
    servers: BTreeMap<String, ServerSpec>,
}

/// In-memory view of the env store, with the disk path it was loaded from.
/// All mutation methods write through to disk before returning.
#[derive(Debug, Clone)]
pub struct EnvStore {
    path: PathBuf,
    data: BTreeMap<String, ServerSpec>,
}

impl EnvStore {
    /// Open `~/.config/edison-stdiod/server_envs.json`. Missing file is fine
    /// and produces an empty store; first write will create it.
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

    /// Look up the full spec for a server, or `None` if nothing is staged.
    pub fn get(&self, server_id: &str) -> Option<&ServerSpec> {
        self.data.get(server_id)
    }

    /// Replace the spec for `server_id` wholesale and flush to disk. Used by
    /// the `ServerSpecUpdate` handler: the backend always pushes the
    /// complete resolved spec, so replacing is correct (and avoids stale
    /// fields from a previous template config).
    pub fn set(&mut self, server_id: &str, spec: ServerSpec) -> Result<()> {
        self.data.insert(server_id.to_string(), spec);
        self.flush()
    }

    /// Merge `env` into the existing entry's env field, overwriting matching
    /// keys and keeping the rest. Creates the entry (with default spec) if
    /// the server has none yet. Used by the legacy `ServerEnvUpdate`
    /// (env-only) path so a partial env push doesn't drop other variables or
    /// stomp on a previously-staged `args`/`command`.
    pub fn merge_env(&mut self, server_id: &str, env: EnvMap) -> Result<()> {
        let entry = self.data.entry(server_id.to_string()).or_default();
        entry.env.extend(env);
        self.flush()
    }

    /// Apply a partial spec from a `ServerSpecUpdate`: both `env` and
    /// `templated_args` are *merged* into the existing entry (matching keys
    /// overwrite, unmentioned keys are kept) so a push that updates one
    /// value doesn't drop the others. Creates the entry with empty defaults
    /// if the server has none yet.
    pub fn merge_template_values(
        &mut self,
        server_id: &str,
        env: Option<EnvMap>,
        templated_args: Option<BTreeMap<String, String>>,
    ) -> Result<()> {
        let entry = self.data.entry(server_id.to_string()).or_default();
        if let Some(env) = env {
            entry.env.extend(env);
        }
        if let Some(ta) = templated_args {
            entry.templated_args.extend(ta);
        }
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

/// Resolve the env map the supervisor should use at spawn.
///
/// The stored env wins when present (the local store is the source of truth
/// for values the user supplied); otherwise we fall back to whatever env the
/// `DesiredServer` itself carries (which today is usually empty - the
/// backend stops emitting env in steady-state pushes).
pub fn resolve_env_for_spawn(stored: Option<&ServerSpec>, fallback: &EnvMap) -> EnvMap {
    match stored {
        Some(spec) if !spec.env.is_empty() => spec.env.clone(),
        _ => fallback.clone(),
    }
}

/// Apply each `(placeholder, value)` substitution to every arg as a naive
/// substring replace. Args that don't reference any placeholder are
/// unchanged. The placeholders include their braces (e.g. `"{PP}"`) - the
/// daemon doesn't parse template syntax, just rewrites the substrings the
/// backend told it about.
pub fn substitute_templated_args(args: &[String], replacements: &BTreeMap<String, String>) -> Vec<String> {
    args.iter()
        .map(|arg| {
            let mut out = arg.clone();
            for (placeholder, value) in replacements {
                out = out.replace(placeholder, value);
            }
            out
        })
        .collect()
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

    fn env_spec(pairs: &[(&str, &str)]) -> ServerSpec {
        ServerSpec {
            env: sample_env(pairs),
            ..Default::default()
        }
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
            .set("srv-1", env_spec(&[("A", "1"), ("B", "2")]))
            .unwrap();
        let got = store.get("srv-1").unwrap();
        assert_eq!(got.env.get("A").map(String::as_str), Some("1"));
        assert_eq!(got.env.get("B").map(String::as_str), Some("2"));
    }

    #[test]
    fn set_persists_across_reopen() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        {
            let mut store = EnvStore::open_at(path.clone()).unwrap();
            store.set("srv-1", env_spec(&[("TOKEN", "abc")])).unwrap();
        }
        let store = EnvStore::open_at(path).unwrap();
        assert_eq!(
            store.get("srv-1").unwrap().env.get("TOKEN").map(String::as_str),
            Some("abc")
        );
    }

    #[test]
    fn set_replaces_existing_spec() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path).unwrap();
        store.set("srv-1", env_spec(&[("OLD", "1")])).unwrap();
        store.set("srv-1", env_spec(&[("NEW", "2")])).unwrap();
        let got = store.get("srv-1").unwrap();
        assert!(got.env.contains_key("NEW"));
        assert!(!got.env.contains_key("OLD"));
    }

    #[test]
    fn merge_env_keeps_unmentioned_keys_and_templated_args() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path).unwrap();
        store
            .set(
                "srv-1",
                ServerSpec {
                    env: sample_env(&[("A", "1"), ("B", "2")]),
                    templated_args: [("{X}".to_string(), "x".to_string())]
                        .into_iter()
                        .collect(),
                },
            )
            .unwrap();
        // Partial env push: update B and add C, leave A + templated_args alone.
        store
            .merge_env("srv-1", sample_env(&[("B", "22"), ("C", "3")]))
            .unwrap();
        let got = store.get("srv-1").unwrap();
        assert_eq!(got.env.get("A").map(String::as_str), Some("1"));
        assert_eq!(got.env.get("B").map(String::as_str), Some("22"));
        assert_eq!(got.env.get("C").map(String::as_str), Some("3"));
        assert_eq!(got.templated_args.get("{X}").map(String::as_str), Some("x"));
    }

    #[test]
    fn merge_template_values_merges_both_maps_and_preserves_unmentioned() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path).unwrap();
        // Existing entry from a prior ServerEnvUpdate.
        store
            .set("fs", env_spec(&[("FASTMCP_LOG_LEVEL", "ERROR")]))
            .unwrap();
        // My MCPs save pushes a {PP} args substitution and a PP env value.
        store
            .merge_template_values(
                "fs",
                Some(sample_env(&[("PP", "/Users/me")])),
                Some([("{PP}".to_string(), "/Users/me".to_string())].into_iter().collect()),
            )
            .unwrap();
        let got = store.get("fs").unwrap();
        // Literal env survives.
        assert_eq!(
            got.env.get("FASTMCP_LOG_LEVEL").map(String::as_str),
            Some("ERROR")
        );
        // New env entry landed.
        assert_eq!(got.env.get("PP").map(String::as_str), Some("/Users/me"));
        // templated_args was set.
        assert_eq!(
            got.templated_args.get("{PP}").map(String::as_str),
            Some("/Users/me")
        );
    }

    #[test]
    fn merge_template_values_can_update_only_args() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path).unwrap();
        store.set("fs", env_spec(&[("M1", "MV1")])).unwrap();
        // Push only templated_args.
        store
            .merge_template_values(
                "fs",
                None,
                Some([("{PP}".to_string(), "/Users/me".to_string())].into_iter().collect()),
            )
            .unwrap();
        let got = store.get("fs").unwrap();
        // env intact.
        assert_eq!(got.env.get("M1").map(String::as_str), Some("MV1"));
        // templated_args set.
        assert_eq!(
            got.templated_args.get("{PP}").map(String::as_str),
            Some("/Users/me")
        );
    }

    #[test]
    fn merge_env_creates_entry_when_missing() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path).unwrap();
        store.merge_env("srv-1", sample_env(&[("X", "1")])).unwrap();
        let got = store.get("srv-1").unwrap();
        assert!(got.templated_args.is_empty());
        assert_eq!(got.env.get("X").map(String::as_str), Some("1"));
    }

    #[test]
    fn substitute_templated_args_rewrites_each_occurrence() {
        let args = vec![
            "-y".to_string(),
            "@mcp/foo".to_string(),
            "{PP}".to_string(),
            "{PP}/bar".to_string(),
        ];
        let subs: BTreeMap<String, String> =
            [("{PP}".to_string(), "/Users/me".to_string())].into_iter().collect();
        let out = substitute_templated_args(&args, &subs);
        assert_eq!(
            out,
            vec![
                "-y".to_string(),
                "@mcp/foo".to_string(),
                "/Users/me".to_string(),
                "/Users/me/bar".to_string(),
            ]
        );
    }

    #[test]
    fn substitute_templated_args_noop_when_no_match() {
        let args = vec!["-y".to_string(), "plain".to_string()];
        let subs: BTreeMap<String, String> =
            [("{NOTHERE}".to_string(), "x".to_string())].into_iter().collect();
        assert_eq!(substitute_templated_args(&args, &subs), args);
    }

    #[test]
    fn remove_drops_entry() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path.clone()).unwrap();
        store.set("srv-1", env_spec(&[("A", "1")])).unwrap();
        store.remove("srv-1").unwrap();
        let store = EnvStore::open_at(path).unwrap();
        assert!(store.get("srv-1").is_none());
    }

    #[test]
    fn remove_missing_server_is_noop() {
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
        store.set("srv-1", env_spec(&[("A", "1")])).unwrap();
        store.set("srv-2", env_spec(&[("B", "2")])).unwrap();
        store.set("srv-3", env_spec(&[("C", "3")])).unwrap();

        let store = EnvStore::open_at(path).unwrap();
        assert_eq!(store.get("srv-1").unwrap().env.get("A").unwrap(), "1");
        assert_eq!(store.get("srv-2").unwrap().env.get("B").unwrap(), "2");
        assert_eq!(store.get("srv-3").unwrap().env.get("C").unwrap(), "3");
    }

    #[test]
    fn no_tmp_file_leftover_after_write() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let mut store = EnvStore::open_at(path.clone()).unwrap();
        store.set("srv-1", env_spec(&[("A", "1")])).unwrap();
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
        store.set("srv-1", env_spec(&[("A", "1")])).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn full_spec_round_trip_through_disk() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        let want = ServerSpec {
            env: sample_env(&[("PP", "/Users/me"), ("TOKEN", "sk-1")]),
            templated_args: [("{PP}".to_string(), "/Users/me".to_string())]
                .into_iter()
                .collect(),
        };
        {
            let mut store = EnvStore::open_at(path.clone()).unwrap();
            store.set("srv-1", want.clone()).unwrap();
        }
        let store = EnvStore::open_at(path).unwrap();
        assert_eq!(store.get("srv-1").cloned(), Some(want));
    }

    #[test]
    fn resolve_env_for_spawn_prefers_stored() {
        let stored = env_spec(&[("PP", "/Users/me")]);
        let fallback = sample_env(&[("PP", "fallback"), ("OTHER", "x")]);
        let got = resolve_env_for_spawn(Some(&stored), &fallback);
        // Stored wins wholesale - we don't merge fallback into it.
        assert_eq!(got.get("PP").map(String::as_str), Some("/Users/me"));
        assert!(!got.contains_key("OTHER"));
    }

    #[test]
    fn resolve_env_for_spawn_falls_back_when_unstored() {
        let fallback = sample_env(&[("X", "1")]);
        let got = resolve_env_for_spawn(None, &fallback);
        assert_eq!(got, fallback);
    }

    #[test]
    fn resolve_env_for_spawn_falls_back_when_stored_env_empty() {
        // A staged spec with only templated_args (no env) shouldn't blank
        // out a DesiredServer that does carry env.
        let stored = ServerSpec {
            templated_args: [("{X}".to_string(), "x".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let fallback = sample_env(&[("Y", "2")]);
        let got = resolve_env_for_spawn(Some(&stored), &fallback);
        assert_eq!(got, fallback);
    }

    #[test]
    fn parse_error_surfaces_path() {
        let dir = tempdir();
        let path = dir.join("server_envs.json");
        std::fs::write(&path, "{ not valid json").unwrap();
        let err = EnvStore::open_at(path).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("failed to parse"), "got: {msg}");
    }
}
