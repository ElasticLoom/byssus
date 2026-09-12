//! `byssus` — the Byssus command-line tool.

fn main() {
    // Entry point only; subcommands are built out in later milestones (see docs/TODO.md).
    #[allow(clippy::print_stderr)]
    {
        eprintln!("byssus {}: not yet implemented", byssus::VERSION);
    }
    std::process::exit(1);
}
