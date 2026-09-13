//! Privilege normalization.
//!
//! [`plan`] decides the credential operations without side effects;
//! [`apply`] performs and verifies them.

pub mod apply;
pub mod plan;
