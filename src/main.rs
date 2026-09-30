//! MAGI: a language for reproducible business-data analysis and reconciliation.

mod ast;
mod backend;
mod cli;
mod diagnostic;
mod export;
mod fmt;
mod lineage;
mod plan;
mod reconcile;
mod semantic;
mod snapshot;
mod source;
mod syntax;
mod testing;

fn main() -> std::process::ExitCode {
    cli::main()
}
