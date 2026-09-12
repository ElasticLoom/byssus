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

pub mod config;
pub mod identity;
pub mod mountinfo;
pub mod name;
pub mod state;
pub mod template;

/// The crate version, as recorded in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
