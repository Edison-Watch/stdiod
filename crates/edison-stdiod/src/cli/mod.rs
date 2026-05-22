//! User-facing CLI subcommands.
//!
//! The `run` subcommand lives in `crate::daemon` so it can stay close to
//! the supervisor loop; everything else (`login`, `install`, `uninstall`,
//! `status`, `logs`, `server …`) is grouped here so the daemon module
//! doesn't accumulate unrelated entry-point glue.

pub mod install;
pub mod login;
pub mod logs;
pub mod server;
pub mod status;
