//! tracing-subscriber setup.

use std::io::IsTerminal;

use anyhow::anyhow;
use tracing_subscriber::EnvFilter;

use crate::config::{LogConfig, LogFormat};

/// Install the global subscriber. The server logs to stdout.
pub fn init(cfg: &LogConfig) -> anyhow::Result<()> {
    let filter = EnvFilter::try_new(&cfg.level)
        .map_err(|e| anyhow!("invalid log.level `{}`: {e}", cfg.level))?;
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stdout);
    match cfg.format {
        LogFormat::Text => builder
            .with_ansi(std::io::stdout().is_terminal())
            .try_init(),
        LogFormat::Json => builder.json().try_init(),
    }
    .map_err(|e| anyhow!("installing tracing subscriber: {e}"))
}

/// Install the global subscriber writing to stderr (offline commands, whose stdout is
/// data: `stats --json`, `export -o -`).
pub fn init_stderr(cfg: &LogConfig) -> anyhow::Result<()> {
    let filter = EnvFilter::try_new(&cfg.level)
        .map_err(|e| anyhow!("invalid log.level `{}`: {e}", cfg.level))?;
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    match cfg.format {
        LogFormat::Text => builder
            .with_ansi(std::io::stderr().is_terminal())
            .try_init(),
        LogFormat::Json => builder.json().try_init(),
    }
    .map_err(|e| anyhow!("installing tracing subscriber: {e}"))
}
