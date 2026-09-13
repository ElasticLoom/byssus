//! Integration tests that perform real mount operations.
//!
//! Every test is `#[ignore]`d so plain `cargo test` never mounts anything.
//! Run them with `scripts/integration-tests.sh`, which executes this binary
//! inside a throwaway user and mount namespace. Each test additionally
//! refuses to run unless it detects that environment.

#![allow(clippy::print_stdout, clippy::print_stderr, unreachable_pub)]

mod common;
mod kernel;
mod privileges;
