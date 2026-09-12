//! Reconciliation of desired membership against state and the kernel.
//!
//! [`plan`] holds the pure decision logic. Gathering observations and
//! executing plans are built on the kernel layer (see `docs/TODO.md`, M3).

pub mod plan;
