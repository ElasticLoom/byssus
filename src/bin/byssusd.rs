//! `byssusd` — the Byssus mount reconciliation daemon.

fn main() {
    // Entry point only; the daemon is built out in later milestones (see docs/TODO.md).
    #[allow(clippy::print_stderr)]
    {
        eprintln!("byssusd {}: not yet implemented", byssus::VERSION);
    }
    std::process::exit(1);
}
