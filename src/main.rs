use std::process::ExitCode;

use clap::Parser;
use ucgeocoder::cli::{self, Cli};

// musl's allocator degrades under multi-threaded load, and the static musl builds (Docker
// image, musl release binaries) are the main deployments. jemalloc was ruled out because
// tikv-jemalloc does not build on native aarch64 musl.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    match cli::run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
