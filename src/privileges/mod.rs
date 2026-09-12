//! Privilege normalization.
//!
//! [`plan`] decides the credential operations without side effects. Applying
//! them and verifying the result is part of the kernel layer (see
//! `docs/TODO.md`, M2).

pub mod plan;
