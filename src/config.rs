//! Layered configuration.
//!
//! Precedence, lowest to highest: built-in defaults < TOML file < `UCG_*` environment
//! variables < CLI flags (`--set key=value` and the named shortcuts) < runtime overrides
//! stored in redb (applied with [`Source::Override`] from Phase 8 on).
//!
//! Every key is declared exactly once in the `define_config!` table below, which generates
//! the typed [`Config`] structs, the [`FIELDS`] metadata (dotted path, env var, default),
//! and the path-based setter used by every layer. The environment is passed in as a
//! closure so tests never have to mutate the process environment (`std::env::set_var`
//! is `unsafe` in edition 2024).

use std::{collections::BTreeMap, fmt, net::SocketAddr, path::PathBuf, time::Duration};

use serde::Serialize;

/// Where the effective value of a key came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Default,
    Toml,
    Env,
    Flag,
    Override,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Default => "default",
            Self::Toml => "toml",
            Self::Env => "env",
            Self::Flag => "flag",
            Self::Override => "override",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("unknown config key `{0}`")]
    UnknownKey(String),
    #[error("invalid value for `{key}` (from {origin}): {msg}")]
    Invalid {
        key: String,
        origin: String,
        msg: String,
    },
    #[error("config file: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("reading config file {path}: {err}")]
    Io { path: PathBuf, err: std::io::Error },
    #[error("config validation failed: {0}")]
    Validation(String),
}

/// An unparsed value from one of the layers.
#[derive(Debug, Clone, Copy)]
pub enum RawValue<'a> {
    Str(&'a str),
    Toml(&'a toml::Value),
}

/// A type that can be used as a config value.
pub trait ConfigValue: Sized {
    fn parse_str(s: &str) -> Result<Self, String>;

    fn from_toml(v: &toml::Value) -> Result<Self, String> {
        match v {
            toml::Value::String(s) => Self::parse_str(s),
            toml::Value::Integer(i) => Self::parse_str(&i.to_string()),
            toml::Value::Float(f) => Self::parse_str(&f.to_string()),
            toml::Value::Boolean(b) => Self::parse_str(&b.to_string()),
            other => Err(format!("unsupported TOML value `{other}`")),
        }
    }

    fn parse_raw(raw: RawValue<'_>) -> Result<Self, String> {
        match raw {
            RawValue::Str(s) => Self::parse_str(s),
            RawValue::Toml(v) => Self::from_toml(v),
        }
    }

    fn render(&self) -> String;
}

macro_rules! from_str_display_value {
    ($($t:ty),*) => {$(
        impl ConfigValue for $t {
            fn parse_str(s: &str) -> Result<Self, String> {
                s.trim().parse::<$t>().map_err(|e| e.to_string())
            }
            fn render(&self) -> String {
                self.to_string()
            }
        }
    )*};
}
from_str_display_value!(u32, u64, usize, SocketAddr);

impl ConfigValue for String {
    fn parse_str(s: &str) -> Result<Self, String> {
        Ok(s.to_owned())
    }
    fn render(&self) -> String {
        self.clone()
    }
}

impl ConfigValue for Option<String> {
    fn parse_str(s: &str) -> Result<Self, String> {
        Ok((!s.is_empty()).then(|| s.to_owned()))
    }
    fn render(&self) -> String {
        self.clone().unwrap_or_default()
    }
}

impl ConfigValue for PathBuf {
    fn parse_str(s: &str) -> Result<Self, String> {
        if s.is_empty() {
            return Err("path must not be empty".into());
        }
        Ok(PathBuf::from(s))
    }
    fn render(&self) -> String {
        self.display().to_string()
    }
}

impl ConfigValue for bool {
    fn parse_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            other => Err(format!("expected a boolean, got `{other}`")),
        }
    }
    fn render(&self) -> String {
        self.to_string()
    }
}

/// Durations use humantime syntax (`90s`, `24h`, `180d`). A bare `0` means
/// [`Duration::ZERO`], which TTL and interval keys interpret as "never" / "off".
impl ConfigValue for Duration {
    fn parse_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s == "0" {
            return Ok(Duration::ZERO);
        }
        humantime::parse_duration(s).map_err(|e| e.to_string())
    }
    fn from_toml(v: &toml::Value) -> Result<Self, String> {
        match v {
            toml::Value::Integer(i) => u64::try_from(*i)
                .map(Duration::from_secs)
                .map_err(|_| "duration must not be negative".into()),
            toml::Value::String(s) => Self::parse_str(s),
            other => Err(format!("expected a duration, got `{other}`")),
        }
    }
    fn render(&self) -> String {
        if self.is_zero() {
            "0".into()
        } else {
            humantime::format_duration(*self).to_string()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Text,
    Json,
}

impl ConfigValue for LogFormat {
    fn parse_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!("expected `text` or `json`, got `{other}`")),
        }
    }
    fn render(&self) -> String {
        match self {
            Self::Text => "text",
            Self::Json => "json",
        }
        .into()
    }
}

/// Static metadata for one config key.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct FieldSpec {
    /// Dotted path, e.g. `throttle.single_rps`; used by TOML, `--set`, and overrides.
    pub path: &'static str,
    pub env: &'static str,
    pub default: &'static str,
    /// Rendered as `***` in any output when set.
    pub secret: bool,
    pub doc: &'static str,
}

macro_rules! define_config {
    (@secret) => { false };
    (@secret $v:literal) => { $v };
    ($(
        $(#[doc = $sdoc:literal])*
        $sec:ident : $Sec:ident {
            $(
                $(#[doc = $doc:literal])*
                $field:ident : $ty:ty = $default:expr, env = $env:literal $(, secret = $secret:literal)? ;
            )*
        }
    )*) => {
        /// Effective configuration. See [`FIELDS`] for per-key metadata.
        #[derive(Debug, Clone, PartialEq)]
        pub struct Config {
            $( $(#[doc = $sdoc])* pub $sec: $Sec, )*
        }

        $(
            $(#[doc = $sdoc])*
            #[derive(Debug, Clone, PartialEq)]
            pub struct $Sec {
                $( $(#[doc = $doc])* pub $field: $ty, )*
            }
        )*

        /// Every config key, in declaration order.
        pub const FIELDS: &[FieldSpec] = &[
            $($(
                FieldSpec {
                    path: concat!(stringify!($sec), ".", stringify!($field)),
                    env: $env,
                    default: $default,
                    secret: define_config!(@secret $($secret)?),
                    doc: concat!($($doc),*),
                },
            )*)*
        ];

        impl Default for Config {
            fn default() -> Self {
                Self {
                    $( $sec: $Sec {
                        $( $field: <$ty as ConfigValue>::parse_str($default).unwrap_or_else(|e| {
                            panic!("bad built-in default for {}.{}: {e}", stringify!($sec), stringify!($field))
                        }), )*
                    }, )*
                }
            }
        }

        impl Config {
            /// Parse `raw` and assign it to the key at `path` (no provenance bookkeeping).
            fn assign(&mut self, path: &str, raw: RawValue<'_>) -> Result<(), SetError> {
                $($(
                    if path == concat!(stringify!($sec), ".", stringify!($field)) {
                        self.$sec.$field = <$ty as ConfigValue>::parse_raw(raw).map_err(SetError::Invalid)?;
                        return Ok(());
                    }
                )*)*
                Err(SetError::Unknown)
            }

            /// Render the current value of the key at `path`, unredacted.
            pub fn render(&self, path: &str) -> Option<String> {
                $($(
                    if path == concat!(stringify!($sec), ".", stringify!($field)) {
                        return Some(ConfigValue::render(&self.$sec.$field));
                    }
                )*)*
                None
            }
        }
    };
}

enum SetError {
    Unknown,
    Invalid(String),
}

define_config! {
    /// HTTP listener.
    server: ServerConfig {
        /// Listen address.
        bind: SocketAddr = "0.0.0.0:8080", env = "UCG_BIND";
        /// Maximum request body size (batch uploads), MiB.
        max_body_mb: u64 = "16", env = "UCG_MAX_BODY_MB";
    }
    /// redb store and its writer task.
    store: StoreConfig {
        /// Directory holding `<cache_name>_v3.redb`.
        db_dir: PathBuf = "./data", env = "UCG_DB_DIR";
        /// Cache name; file is `<cache_name>_v3.redb` (cached-compatible).
        cache_name: String = "geocode-census", env = "UCG_CACHE_NAME";
        /// redb page cache size, MiB.
        db_cache_mb: u64 = "256", env = "UCG_DB_CACHE_MB";
        /// Writer batching window, ms.
        writer_flush_ms: u64 = "50", env = "UCG_WRITER_FLUSH_MS";
        /// Maximum puts per write transaction.
        writer_max_batch: usize = "500", env = "UCG_WRITER_MAX_BATCH";
        /// Interval between durable (fsync) commits, seconds.
        durable_commit_secs: u64 = "30", env = "UCG_DURABLE_COMMIT_SECS";
        /// Interval for flushing in-memory hit counters, seconds.
        hitcount_flush_secs: u64 = "60", env = "UCG_HITCOUNT_FLUSH_SECS";
    }
    /// Upstream Census endpoints.
    upstream: UpstreamConfig {
        /// Census Geocoder base URL.
        url: String = "https://geocoding.geo.census.gov/geocoder", env = "UCG_UPSTREAM_URL";
        /// TIGERweb REST services base URL.
        tigerweb_url: String = "https://tigerweb.geo.census.gov/arcgis/rest/services/TIGERweb", env = "UCG_TIGERWEB_URL";
        /// User-Agent sent upstream.
        user_agent: String = concat!("ucgeocoder/", env!("CARGO_PKG_VERSION"), " (+", env!("CARGO_PKG_REPOSITORY"), ")"), env = "UCG_USER_AGENT";
        /// Single-record request timeout, seconds.
        timeout_secs: u64 = "30", env = "UCG_UPSTREAM_TIMEOUT_SECS";
        /// Batch upload timeout, seconds.
        batch_timeout_secs: u64 = "600", env = "UCG_UPSTREAM_BATCH_TIMEOUT_SECS";
    }
    /// Cache behaviour.
    cache: CacheConfig {
        /// Always fetch `geographies&layers=all` upstream and serve every view from it.
        fetch_geographies_always: bool = "true", env = "UCG_FETCH_GEOGRAPHIES_ALWAYS";
        /// Serve expired entries (`X-Cache: STALE`) when upstream fails.
        serve_stale_on_error: bool = "true", env = "UCG_SERVE_STALE_ON_ERROR";
    }
    /// Upstream throttle, retry and circuit breaker.
    throttle: ThrottleConfig {
        /// Single-record requests per second.
        single_rps: u32 = "5", env = "UCG_SINGLE_RPS";
        /// Single-record burst size.
        single_burst: u32 = "1", env = "UCG_SINGLE_BURST";
        /// Concurrent single-record requests.
        single_concurrency: usize = "4", env = "UCG_SINGLE_CONCURRENCY";
        /// Concurrent batch uploads.
        batch_concurrency: usize = "1", env = "UCG_BATCH_CONCURRENCY";
        /// Minimum gap between batch uploads, seconds.
        batch_min_interval_secs: u64 = "2", env = "UCG_BATCH_MIN_INTERVAL_SECS";
        /// Background (job) acquire poll interval, ms.
        background_poll_ms: u64 = "100", env = "UCG_BACKGROUND_POLL_MS";
        /// Retries on transient upstream errors.
        max_retries: u32 = "3", env = "UCG_MAX_RETRIES";
        /// Exponential backoff base, ms.
        retry_base_ms: u64 = "1000", env = "UCG_RETRY_BASE_MS";
        /// Backoff / Retry-After cap, seconds.
        retry_max_secs: u64 = "60", env = "UCG_RETRY_MAX_SECS";
        /// Consecutive transient failures that open the breaker.
        breaker_threshold: u32 = "10", env = "UCG_BREAKER_THRESHOLD";
        /// Initial breaker open period, seconds (doubles up to 15 min).
        breaker_open_secs: u64 = "60", env = "UCG_BREAKER_OPEN_SECS";
    }
    /// Entry time-to-live by status and benchmark class (`0` = never expires).
    ttl: TtlConfig {
        /// Match on a `Current`-class benchmark.
        match_current: Duration = "180d", env = "UCG_TTL_MATCH_CURRENT";
        /// Match on a `Fixed`-class benchmark (e.g. Census2020).
        match_fixed: Duration = "0", env = "UCG_TTL_MATCH_FIXED";
        /// Tie.
        tie: Duration = "30d", env = "UCG_TTL_TIE";
        /// NoMatch from a single-record lookup.
        nomatch_single: Duration = "30d", env = "UCG_TTL_NOMATCH_SINGLE";
        /// NoMatch from a batch or job (also queued for retry).
        nomatch_batch: Duration = "24h", env = "UCG_TTL_NOMATCH_BATCH";
        /// Benchmarks / vintages / layers catalog.
        catalog: Duration = "24h", env = "UCG_TTL_CATALOG";
    }
    /// Re-checking of batch NoMatch results.
    retry: RetryConfig {
        /// Delay before a batch NoMatch is retried.
        negative_delay: Duration = "1h", env = "UCG_NEGATIVE_RETRY_DELAY";
        /// Maximum retry attempts per NoMatch entry.
        negative_attempts: u32 = "3", env = "UCG_NEGATIVE_RETRY_ATTEMPTS";
        /// Interval of the automatic refresh-negatives job (`0` = off).
        auto_refresh_interval: Duration = "6h", env = "UCG_NEGATIVE_AUTO_REFRESH";
    }
    /// Synchronous batch endpoints.
    batch: BatchConfig {
        /// Rows per upstream batch upload.
        chunk_size: usize = "2000", env = "UCG_BATCH_CHUNK_SIZE";
        /// Maximum rows accepted per request.
        max_rows: usize = "10000", env = "UCG_BATCH_MAX_ROWS";
        /// Send heuristically-matched rows upstream when the limiter is idle.
        upgrade_when_idle: bool = "false", env = "UCG_BATCH_UPGRADE_WHEN_IDLE";
        /// Synthesize partial single-record JSON from batch-only entries.
        serve_partial_json_from_batch: bool = "false", env = "UCG_SERVE_PARTIAL_JSON_FROM_BATCH";
    }
    /// Async background jobs.
    jobs: JobsConfig {
        /// Rows per job chunk.
        chunk_size: usize = "2000", env = "UCG_JOBS_CHUNK_SIZE";
        /// Attempts per chunk before its rows are marked failed.
        chunk_max_attempts: u32 = "3", env = "UCG_JOBS_CHUNK_MAX_ATTEMPTS";
        /// In-flight chunk lease, seconds.
        lease_secs: u64 = "900", env = "UCG_JOBS_LEASE_SECS";
        /// Re-fetch batch matches via single-record when idle to fill the JSON view.
        single_record_upgrade: bool = "false", env = "UCG_JOBS_SINGLE_RECORD_UPGRADE";
    }
    /// Admin API authentication.
    admin: AdminConfig {
        /// Bearer token for `/api/v1`, `/admin`, `/metrics`; unset = open.
        token: Option<String> = "", env = "UCG_ADMIN_TOKEN", secret = true;
        /// Refuse to start without a token.
        require_token: bool = "false", env = "UCG_ADMIN_REQUIRE_TOKEN";
        /// Serve `/metrics` without a token.
        metrics_public: bool = "false", env = "UCG_METRICS_PUBLIC";
    }
    /// Logging.
    log: LogConfig {
        /// tracing-subscriber EnvFilter directive.
        level: String = "info", env = "UCG_LOG";
        /// `text` or `json`.
        format: LogFormat = "text", env = "UCG_LOG_FORMAT";
    }
}

/// Per-key record of which layer supplied the effective value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance(BTreeMap<&'static str, Source>);

impl Default for Provenance {
    fn default() -> Self {
        Self(FIELDS.iter().map(|f| (f.path, Source::Default)).collect())
    }
}

impl Provenance {
    pub fn get(&self, path: &str) -> Option<Source> {
        self.0.get(path).copied()
    }
}

/// Inputs for [`load`]. `env` is a lookup function so tests can inject variables.
pub struct Layers<'a> {
    /// Contents of the TOML file and the path it came from (for error messages).
    pub toml: Option<(&'a str, &'a str)>,
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// `(dotted path, value)` pairs from CLI flags, applied in order.
    pub flags: &'a [(String, String)],
}

/// Effective config plus where each value came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub config: Config,
    pub provenance: Provenance,
}

impl Settings {
    /// Apply one value from `origin` to the key at `path`.
    pub fn set(
        &mut self,
        path: &str,
        raw: RawValue<'_>,
        origin: &str,
        source: Source,
    ) -> Result<(), ConfigError> {
        match self.config.assign(path, raw) {
            Ok(()) => {
                let spec = FIELDS
                    .iter()
                    .find(|f| f.path == path)
                    .expect("assigned key has a spec");
                self.provenance.0.insert(spec.path, source);
                Ok(())
            }
            Err(SetError::Unknown) => Err(ConfigError::UnknownKey(path.to_owned())),
            Err(SetError::Invalid(msg)) => Err(ConfigError::Invalid {
                key: path.to_owned(),
                origin: origin.to_owned(),
                msg,
            }),
        }
    }

    /// One row per key, secrets redacted; used by `check-config` and the admin API.
    pub fn describe(&self) -> Vec<FieldReport> {
        FIELDS
            .iter()
            .map(|f| {
                let value = self.config.render(f.path).unwrap_or_default();
                let value = if f.secret && !value.is_empty() {
                    "***".to_owned()
                } else {
                    value
                };
                FieldReport {
                    path: f.path,
                    value,
                    source: self.provenance.get(f.path).unwrap_or(Source::Default),
                    env: f.env,
                    default: f.default,
                    doc: f.doc.trim(),
                }
            })
            .collect()
    }
}

/// Rendered view of one key.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct FieldReport {
    pub path: &'static str,
    pub value: String,
    pub source: Source,
    pub env: &'static str,
    pub default: &'static str,
    pub doc: &'static str,
}

/// Build the effective config from defaults, TOML, env and flags.
pub fn load(layers: &Layers<'_>) -> Result<Settings, ConfigError> {
    let mut s = Settings {
        config: Config::default(),
        provenance: Provenance::default(),
    };

    if let Some((text, file)) = layers.toml {
        let table: toml::Table = text.parse()?;
        for (section, value) in &table {
            let toml::Value::Table(inner) = value else {
                return Err(ConfigError::UnknownKey(section.clone()));
            };
            for (key, v) in inner {
                s.set(
                    &format!("{section}.{key}"),
                    RawValue::Toml(v),
                    file,
                    Source::Toml,
                )?;
            }
        }
    }

    for f in FIELDS {
        if let Some(v) = (layers.env)(f.env).filter(|v| !v.is_empty()) {
            s.set(f.path, RawValue::Str(&v), f.env, Source::Env)?;
        }
    }

    for (path, v) in layers.flags {
        s.set(path, RawValue::Str(v), "command line", Source::Flag)?;
    }

    s.config.validate()?;
    Ok(s)
}

/// Sections a runtime override (`PUT /api/v1/config`) may change. The listener, store,
/// upstream endpoints, admin auth and logging are fixed at startup, so overriding them
/// would silently do nothing; they are refused instead.
pub const OVERRIDABLE_SECTIONS: &[&str] = &["cache", "ttl", "retry", "batch", "jobs", "throttle"];

pub fn is_overridable(path: &str) -> bool {
    path.split_once('.')
        .is_some_and(|(section, _)| OVERRIDABLE_SECTIONS.contains(&section))
        && FIELDS.iter().any(|f| f.path == path)
}

/// The text a key parses from a stored JSON override value (string, number or bool).
pub fn override_text(json: &str) -> Result<String, String> {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::String(s)) => Ok(s),
        Ok(v @ (serde_json::Value::Number(_) | serde_json::Value::Bool(_))) => Ok(v.to_string()),
        Ok(other) => Err(format!(
            "expected a string, number or boolean, got `{other}`"
        )),
        Err(e) => Err(e.to_string()),
    }
}

impl Settings {
    /// These settings with runtime overrides (`(dotted path, JSON value)`) applied as
    /// [`Source::Override`], then validated.
    pub fn with_overrides<'a>(
        &self,
        overrides: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, ConfigError> {
        let mut s = self.clone();
        for (path, json) in overrides {
            let invalid = |msg: String| ConfigError::Invalid {
                key: path.to_owned(),
                origin: "runtime override".into(),
                msg,
            };
            if !FIELDS.iter().any(|f| f.path == path) {
                return Err(ConfigError::UnknownKey(path.to_owned()));
            }
            if !is_overridable(path) {
                return Err(invalid("cannot be changed at runtime".into()));
            }
            let text = override_text(json).map_err(invalid)?;
            s.set(
                path,
                RawValue::Str(&text),
                "runtime override",
                Source::Override,
            )?;
        }
        s.config.validate()?;
        Ok(s)
    }
}

impl Config {
    /// Cross-field and range checks that parsing alone cannot express.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errs = Vec::new();
        let t = &self.throttle;
        if t.single_rps == 0 {
            errs.push("throttle.single_rps must be > 0");
        }
        if t.single_burst == 0 {
            errs.push("throttle.single_burst must be > 0");
        }
        if t.single_concurrency == 0 || t.batch_concurrency == 0 {
            errs.push("throttle concurrency values must be > 0");
        }
        if !(1..=9999).contains(&self.batch.chunk_size) {
            errs.push("batch.chunk_size must be in 1..=9999");
        }
        if !(1..=9999).contains(&self.jobs.chunk_size) {
            errs.push("jobs.chunk_size must be in 1..=9999");
        }
        if self.batch.max_rows == 0 {
            errs.push("batch.max_rows must be > 0");
        }
        if self.admin.require_token && self.admin.token.is_none() {
            errs.push("admin.require_token is set but admin.token (UCG_ADMIN_TOKEN) is not");
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Validation(errs.join("; ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn defaults_parse_and_match_plan() {
        let c = Config::default();
        assert_eq!(c.server.bind, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(c.throttle.single_rps, 5);
        assert_eq!(c.throttle.single_burst, 1);
        assert_eq!(c.ttl.match_current, Duration::from_secs(180 * 86_400));
        assert_eq!(c.ttl.match_fixed, Duration::ZERO);
        assert_eq!(c.ttl.nomatch_batch, Duration::from_secs(86_400));
        assert_eq!(c.admin.token, None);
        assert_eq!(c.log.format, LogFormat::Text);
        assert!(c.upstream.user_agent.starts_with("ucgeocoder/"));
        c.validate().unwrap();
    }

    #[test]
    fn every_field_has_unique_path_and_env() {
        let mut paths: Vec<_> = FIELDS.iter().map(|f| f.path).collect();
        let mut envs: Vec<_> = FIELDS.iter().map(|f| f.env).collect();
        paths.sort_unstable();
        envs.sort_unstable();
        paths.dedup();
        envs.dedup();
        assert_eq!(paths.len(), FIELDS.len());
        assert_eq!(envs.len(), FIELDS.len());
        assert!(FIELDS.iter().all(|f| f.env.starts_with("UCG_")));
    }

    #[test]
    fn precedence_default_toml_env_flag() {
        let toml = r#"
            [server]
            bind = "127.0.0.1:9000"
            [throttle]
            single_rps = 7
            batch_concurrency = 2
            [ttl]
            tie = "10d"
        "#;
        let env = env_from(&[("UCG_SINGLE_RPS", "9"), ("UCG_TTL_TIE", "20d")]);
        let flags = vec![("ttl.tie".to_owned(), "30m".to_owned())];
        let s = load(&Layers {
            toml: Some((toml, "test.toml")),
            env: &env,
            flags: &flags,
        })
        .unwrap();
        let c = &s.config;
        let p = &s.provenance;

        assert_eq!(c.throttle.single_burst, 1);
        assert_eq!(p.get("throttle.single_burst"), Some(Source::Default));

        assert_eq!(c.server.bind, "127.0.0.1:9000".parse().unwrap());
        assert_eq!(c.throttle.batch_concurrency, 2);
        assert_eq!(p.get("server.bind"), Some(Source::Toml));

        assert_eq!(c.throttle.single_rps, 9);
        assert_eq!(p.get("throttle.single_rps"), Some(Source::Env));

        assert_eq!(c.ttl.tie, Duration::from_secs(30 * 60));
        assert_eq!(p.get("ttl.tie"), Some(Source::Flag));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = load(&Layers {
            toml: Some(("[throttle]\nsingle_rpz = 1\n", "t.toml")),
            env: &no_env,
            flags: &[],
        })
        .unwrap_err();
        assert!(matches!(err, ConfigError::UnknownKey(k) if k == "throttle.single_rpz"));

        let err = load(&Layers {
            toml: None,
            env: &no_env,
            flags: &[("nope.key".into(), "1".into())],
        })
        .unwrap_err();
        assert!(matches!(err, ConfigError::UnknownKey(_)));
    }

    #[test]
    fn invalid_env_value_names_the_variable() {
        let env = env_from(&[("UCG_SINGLE_RPS", "fast")]);
        let err = load(&Layers {
            toml: None,
            env: &env,
            flags: &[],
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("throttle.single_rps") && msg.contains("UCG_SINGLE_RPS"),
            "{msg}"
        );
    }

    #[test]
    fn empty_env_is_unset() {
        let env = env_from(&[("UCG_SINGLE_RPS", "")]);
        let s = load(&Layers {
            toml: None,
            env: &env,
            flags: &[],
        })
        .unwrap();
        assert_eq!(
            s.provenance.get("throttle.single_rps"),
            Some(Source::Default)
        );
    }

    #[test]
    fn durations_accept_zero_and_toml_integers() {
        let s = load(&Layers {
            toml: Some(("[ttl]\ncatalog = 3600\n", "t.toml")),
            env: &env_from(&[("UCG_TTL_MATCH_CURRENT", "0")]),
            flags: &[],
        })
        .unwrap();
        assert_eq!(s.config.ttl.catalog, Duration::from_secs(3600));
        assert_eq!(s.config.ttl.match_current, Duration::ZERO);
        assert_eq!(s.config.render("ttl.match_current").as_deref(), Some("0"));
        assert_eq!(s.config.render("ttl.catalog").as_deref(), Some("1h"));
    }

    #[test]
    fn validation_catches_bad_ranges_and_missing_token() {
        let flags = vec![("batch.chunk_size".to_owned(), "10000".to_owned())];
        let err = load(&Layers {
            toml: None,
            env: &no_env,
            flags: &flags,
        })
        .unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));

        let env = env_from(&[("UCG_ADMIN_REQUIRE_TOKEN", "true")]);
        let err = load(&Layers {
            toml: None,
            env: &env,
            flags: &[],
        })
        .unwrap_err();
        assert!(err.to_string().contains("admin.token"));
    }

    #[test]
    fn describe_redacts_secrets() {
        let env = env_from(&[("UCG_ADMIN_TOKEN", "hunter2")]);
        let s = load(&Layers {
            toml: None,
            env: &env,
            flags: &[],
        })
        .unwrap();
        let rows = s.describe();
        let tok = rows.iter().find(|r| r.path == "admin.token").unwrap();
        assert_eq!(tok.value, "***");
        assert_eq!(tok.source, Source::Env);
        assert!(rows.iter().all(|r| !r.value.contains("hunter2")));
        assert_eq!(rows.len(), FIELDS.len());
    }

    #[test]
    fn example_toml_is_complete_and_matches_defaults() {
        let text = include_str!("../ucgeocoder.example.toml");
        let s = load(&Layers {
            toml: Some((text, "ucgeocoder.example.toml")),
            env: &no_env,
            flags: &[],
        })
        .unwrap();
        assert_eq!(s.config, Config::default());
        let commented = ["admin.token", "upstream.user_agent"];
        for f in FIELDS.iter().filter(|f| !commented.contains(&f.path)) {
            assert_eq!(
                s.provenance.get(f.path),
                Some(Source::Toml),
                "{} missing from example",
                f.path
            );
        }
    }

    #[test]
    fn runtime_overrides() {
        let base = load(&Layers {
            toml: None,
            env: &no_env,
            flags: &[],
        })
        .unwrap();
        let s = base
            .with_overrides([
                ("throttle.single_rps", "2"),
                ("ttl.tie", "\"7d\""),
                ("cache.serve_stale_on_error", "false"),
            ])
            .unwrap();
        assert_eq!(s.config.throttle.single_rps, 2);
        assert_eq!(s.config.ttl.tie, Duration::from_secs(7 * 86_400));
        assert!(!s.config.cache.serve_stale_on_error);
        assert_eq!(
            s.provenance.get("throttle.single_rps"),
            Some(Source::Override)
        );
        assert_eq!(
            s.provenance.get("throttle.single_burst"),
            Some(Source::Default)
        );

        assert!(matches!(
            base.with_overrides([("server.bind", "\"0.0.0.0:1\"")]),
            Err(ConfigError::Invalid { .. })
        ));
        assert!(matches!(
            base.with_overrides([("nope.key", "1")]),
            Err(ConfigError::UnknownKey(_))
        ));
        assert!(matches!(
            base.with_overrides([("throttle.single_rps", "0")]),
            Err(ConfigError::Validation(_))
        ));
        assert!(
            base.with_overrides([("throttle.single_rps", "[1]")])
                .is_err()
        );
        assert!(is_overridable("jobs.chunk_size"));
        assert!(!is_overridable("admin.token"));
        assert!(!is_overridable("throttle.nope"));
    }
}
