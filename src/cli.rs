//! Command-line interface.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::{
    VERSION,
    config::{self, ConfigError, Layers, Settings},
    http, telemetry,
};

#[derive(Debug, Parser)]
#[command(
    name = "ucgeocoder",
    version = VERSION,
    about = "Caching, rate-limited, wire-compatible proxy for the US Census Geocoder"
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Options shared by every subcommand. Each maps onto a config key and is applied
/// with flag precedence (above TOML and `UCG_*` env vars).
#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// TOML config file [env: UCG_CONFIG]
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Listen address (server.bind)
    #[arg(long, global = true, value_name = "ADDR")]
    pub bind: Option<String>,
    /// Database directory (store.db_dir)
    #[arg(long, global = true, value_name = "DIR")]
    pub db_dir: Option<String>,
    /// Log filter directive (log.level)
    #[arg(long, global = true, value_name = "FILTER")]
    pub log: Option<String>,
    /// Set any config key, e.g. `--set throttle.single_rps=2` (repeatable)
    #[arg(long = "set", short = 's', global = true, value_name = "KEY=VALUE", value_parser = parse_key_value)]
    pub set: Vec<(String, String)>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Run the server (default)
    Serve,
    /// Print the effective config with the source of each value, then exit
    CheckConfig {
        /// Emit JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Train the zstd dictionary from captured upstream responses (dev tool)
    TrainDict {
        /// Directory of raw single-record JSON responses (scripts/capture.py samples)
        #[arg(long, value_name = "DIR")]
        samples: PathBuf,
        /// Output dictionary path
        #[arg(
            short,
            long,
            value_name = "PATH",
            default_value = "resources/census.dict"
        )]
        output: PathBuf,
        /// Maximum dictionary size in bytes
        #[arg(long, default_value_t = crate::dict::train::DEFAULT_MAX_SIZE)]
        max_size: usize,
    },
    /// Cache statistics (offline; the server must be stopped)
    Stats {
        /// Emit JSON
        #[arg(long)]
        json: bool,
        /// Also scan every entry header (counts by status/searchtype/benchmark, compression)
        #[arg(long)]
        full: bool,
    },
    /// Delete cache entries by scope (offline)
    Purge {
        /// all | expired | benchmark | vintage | status | prefix
        #[arg(long)]
        scope: String,
        /// benchmark: id or name; vintage: `benchmark:vintage`; status: Match/NoMatch/Tie;
        /// prefix: the key prefix (matched exactly)
        #[arg(long)]
        value: Option<String>,
        /// Only count what would be deleted
        #[arg(long)]
        dry_run: bool,
    },
    /// Export cache entries as JSON lines (offline)
    Export {
        /// Only keys starting with this prefix
        #[arg(long, default_value = "")]
        prefix: String,
        /// Output file (`-` for stdout)
        #[arg(short, long, value_name = "PATH")]
        output: PathBuf,
    },
    /// Restore exported JSON lines, or queue a CSV as a cache-warming job (offline)
    Import {
        /// `.jsonl` export, or a `.csv`/`.xlsx` batch file
        file: PathBuf,
        /// jsonl or csv (default: from the file extension)
        #[arg(long)]
        format: Option<String>,
        /// csv: locations | geographies
        #[arg(long, default_value = "locations")]
        returntype: String,
        /// csv: benchmark id or name
        #[arg(long)]
        benchmark: Option<String>,
        /// csv: vintage id or name (required for geographies)
        #[arg(long)]
        vintage: Option<String>,
        /// csv: address | onelineaddress | coordinates
        #[arg(long, default_value = "address")]
        searchtype: String,
    },
    /// Reclaim free space in the database file (offline)
    Compact,
    /// Show or refresh the benchmark/vintage catalog (offline)
    Catalog {
        #[command(subcommand)]
        action: CatalogAction,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum CatalogAction {
    /// Print the catalog in effect
    Show {
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Fetch benchmarks and vintages from upstream into the store, then print them
    Refresh {
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
}

fn parse_key_value(s: &str) -> Result<(String, String), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VALUE, got `{s}`"))?;
    let k = k.trim();
    if k.is_empty() {
        return Err(format!("empty key in `{s}`"));
    }
    Ok((k.to_owned(), v.to_owned()))
}

impl GlobalArgs {
    /// Named flags first, then `--set` pairs in command-line order.
    pub fn flag_pairs(&self) -> Vec<(String, String)> {
        let named = [
            ("server.bind", &self.bind),
            ("store.db_dir", &self.db_dir),
            ("log.level", &self.log),
        ];
        named
            .into_iter()
            .filter_map(|(k, v)| v.clone().map(|v| (k.to_owned(), v)))
            .chain(self.set.iter().cloned())
            .collect()
    }

    /// Load settings from the config file (flag or `UCG_CONFIG`), `env`, and flags.
    pub fn load_settings(
        &self,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Settings, ConfigError> {
        let path = self.config.clone().or_else(|| {
            env("UCG_CONFIG")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        });
        let text = path
            .as_ref()
            .map(|p| {
                std::fs::read_to_string(p).map_err(|err| ConfigError::Io {
                    path: p.clone(),
                    err,
                })
            })
            .transpose()?;
        let path_str = path.as_ref().map(|p| p.display().to_string());
        config::load(&Layers {
            toml: text.as_deref().zip(path_str.as_deref()),
            env,
            flags: &self.flag_pairs(),
        })
    }
}

pub fn run(cli: Cli) -> anyhow::Result<()> {
    let env = |k: &str| std::env::var(k).ok();
    let settings = cli.global.load_settings(&env)?;
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            telemetry::init(&settings.config.log)?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(http::serve(settings))
        }
        Command::CheckConfig { json } => {
            print_config(&settings, json)?;
            Ok(())
        }
        Command::TrainDict {
            samples,
            output,
            max_size,
        } => {
            let report = crate::dict::train::run(&samples, &output, max_size)?;
            println!("{report}\nwrote {}", output.display());
            Ok(())
        }
        offline => {
            telemetry::init_stderr(&settings.config.log)?;
            run_offline(settings, offline)
        }
    }
}

fn run_offline(settings: Settings, command: Command) -> anyhow::Result<()> {
    use crate::offline;
    match command {
        Command::Stats { json, full } => offline::stats(settings, json, full),
        Command::Purge {
            scope,
            value,
            dry_run,
        } => offline::purge_cmd(settings, &scope, value.as_deref(), dry_run),
        Command::Export { prefix, output } => offline::export(settings, &prefix, &output),
        Command::Import {
            file,
            format,
            returntype,
            benchmark,
            vintage,
            searchtype,
        } => {
            let format = format.unwrap_or_else(|| {
                let ext = file
                    .extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                if ["csv", "xls", "xlsx", "xlsm", "xlsb"].contains(&ext.as_str()) {
                    "csv".into()
                } else {
                    "jsonl".into()
                }
            });
            let csv = match format.as_str() {
                "jsonl" => None,
                "csv" => Some(offline::CsvImport {
                    returntype,
                    benchmark,
                    vintage,
                    searchtype,
                }),
                other => anyhow::bail!("unknown import format `{other}` (jsonl or csv)"),
            };
            offline::import(settings, &file, csv)
        }
        Command::Compact => offline::compact_cmd(settings),
        Command::Catalog { action } => match action {
            CatalogAction::Show { json } => offline::catalog(settings, false, json),
            CatalogAction::Refresh { json } => offline::catalog(settings, true, json),
        },
        Command::Serve | Command::CheckConfig { .. } | Command::TrainDict { .. } => {
            unreachable!("handled by run")
        }
    }
}

fn print_config(settings: &Settings, json: bool) -> anyhow::Result<()> {
    let rows = settings.describe();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    let pw = rows.iter().map(|r| r.path.len()).max().unwrap_or(0);
    let vw = rows
        .iter()
        .map(|r| r.value.len())
        .max()
        .unwrap_or(0)
        .min(48);
    println!("{:pw$}  {:vw$}  SOURCE    ENV", "KEY", "VALUE");
    for r in rows {
        println!(
            "{:pw$}  {:vw$}  {:8}  {}",
            r.path,
            r.value,
            r.source.to_string(),
            r.env
        );
    }
    Ok(())
}
