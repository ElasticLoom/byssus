//! Byssus creates live, read-only filesystem attachments between isolated
//! workspaces.
//!
//! The `byssusd` daemon watches membership directories and reconciles the
//! kernel mount table against them, creating and removing read-only bind
//! mounts beneath a configured target root. Mount propagation carries those
//! mounts into already-running containers.
//!
//! This library holds everything shared by the `byssusd` daemon and the
//! `byssus` CLI. See `docs/DESIGN.md` in the repository for the full design
//! and security contract.

#[cfg(not(target_os = "linux"))]
compile_error!("byssus relies on Linux-specific mount APIs and only builds on Linux");

pub mod app;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod fsops;
pub mod identity;
pub mod lock;
pub mod logging;
pub mod membership;
pub mod mount;
pub mod mountinfo;
pub mod name;
pub mod notify;
pub mod privileges;
pub mod probe;
pub mod reconcile;
pub mod report;
pub mod runtime;
pub mod signals;
pub mod state;
pub mod sys;
pub mod template;
pub mod users;
pub mod watcher;

/// The crate version, as recorded in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
