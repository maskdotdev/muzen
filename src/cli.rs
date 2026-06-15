use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::reviewer_kernel::system::redact_known_secrets;

#[derive(Parser, Debug)]
#[command(name = "muzen")]
#[command(about = "Internal Muzen diagnostics and operations CLI")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// Inspect Muzen Context Engine output for a local snapshot.
    Context(crate::context_engine::cli::ContextArgs),
    /// Compose, validate, or inspect operational proof evidence.
    Proof(crate::operational_proof::ProofArgs),
}

pub fn main_entry() {
    let code = match run_main() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{}", redact_known_secrets(&format!("{error:#}"), &[]));
            4
        }
    };
    std::process::exit(code);
}

pub(crate) fn run_main() -> Result<i32> {
    let cli = Cli::parse();
    match cli.command {
        Command::Context(args) => crate::context_engine::cli::run_context(args),
        Command::Proof(args) => crate::operational_proof::run_proof(args),
    }
}
