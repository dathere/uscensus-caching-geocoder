use std::process::ExitCode;

use clap::Parser;
use ucgeocoder::cli::{self, Cli};

fn main() -> ExitCode {
    match cli::run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
