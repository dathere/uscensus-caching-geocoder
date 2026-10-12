//! Cache maintenance shared by the admin API and (later) the offline CLI: purge scopes
//! and the export/import record format.
//!
//! Purge decides from the 17-byte header and `created_at` only (never a payload decode),
//! because the real purge runs on the single writer thread. The dry run uses the same
//! predicate in a read transaction, so its count is what the purge would delete.
//!
//! Export writes the *decoded* entry (the upstream `json` segment verbatim as raw JSON),
//! not the stored bytes: those carry a dictionary id, and a later build may no longer
//! have that dictionary. Import re-encodes with the current codec and keeps `created_at`.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use serde_json::value::RawValue;

use super::{
    codec::Codec,
    envelope::{
        BatchView, Entry, EntryPayload, EntrySource, EnvelopeError, Header, Status, Views,
        decode_stored, encode_stored,
    },
    key::{CacheKey, KEY_VERSION, SearchType},
    policy::{BenchmarkClass, Freshness, freshness, ttl},
};
use crate::{
    config::TtlConfig,
    store::{Store, StoreError},
};

/// What `POST /api/v1/cache/purge` removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PurgeScope {
    All,
    /// Past their TTL (as `policy::ttl` computes it; a fixed-class Match never expires).
    Expired,
    Benchmark(u32),
    Vintage {
        benchmark_id: u32,
        vintage_id: u32,
    },
    Status(Status),
    /// Keys starting with this text.
    Prefix(String),
}

impl PurgeScope {
    /// Key prefixes the scope is confined to (empty: the whole table).
    pub fn prefixes(&self) -> Vec<String> {
        match self {
            Self::Benchmark(b) => SearchType::ALL
                .iter()
                .map(|st| CacheKey::benchmark_prefix(*st, *b))
                .collect(),
            Self::Vintage {
                benchmark_id,
                vintage_id,
            } => SearchType::ALL
                .iter()
                .map(|st| format!("{KEY_VERSION}|{st}|{benchmark_id}|{vintage_id}|"))
                .collect(),
            Self::Prefix(p) => vec![p.clone()],
            Self::All | Self::Expired | Self::Status(_) => Vec::new(),
        }
    }
}

/// Inputs to the expiry decision, fixed for one purge.
#[derive(Debug, Clone)]
pub struct PurgeCtx {
    pub ttl: TtlConfig,
    pub classes: HashMap<u32, BenchmarkClass>,
    pub now: SystemTime,
}

fn header_of(bytes: &[u8]) -> Option<(Header, SystemTime)> {
    let (envelope, created_at) = decode_stored(bytes).ok()?;
    Some((Header::parse(&envelope).ok()?, created_at))
}

/// Does `scope` remove this entry? Undecodable entries count as expired (they are
/// unusable) and match no status.
pub fn purge_matches(scope: &PurgeScope, ctx: &PurgeCtx, key: &str, bytes: &[u8]) -> bool {
    match scope {
        PurgeScope::All => true,
        PurgeScope::Prefix(p) => key.starts_with(p.as_str()),
        PurgeScope::Benchmark(b) => CacheKey::parse(key).is_ok_and(|k| k.benchmark_id == *b),
        PurgeScope::Vintage {
            benchmark_id,
            vintage_id,
        } => CacheKey::parse(key)
            .is_ok_and(|k| k.benchmark_id == *benchmark_id && k.vintage_id == *vintage_id),
        PurgeScope::Status(s) => header_of(bytes).is_some_and(|(h, _)| h.status == *s),
        PurgeScope::Expired => {
            let Some((h, created_at)) = header_of(bytes) else {
                return true;
            };
            let class = CacheKey::parse(key)
                .ok()
                .and_then(|k| ctx.classes.get(&k.benchmark_id).copied())
                .unwrap_or(BenchmarkClass::Current);
            freshness(
                created_at,
                ctx.now,
                ttl(&ctx.ttl, h.status, h.source, class),
                false,
            ) != Freshness::Fresh
        }
    }
}

/// Count (`dry_run`) or delete the entries `scope` selects.
pub async fn purge(
    store: &Store,
    scope: PurgeScope,
    ctx: PurgeCtx,
    dry_run: bool,
) -> Result<u64, StoreError> {
    let prefixes = scope.prefixes();
    if dry_run {
        let store = store.clone();
        return tokio::task::spawn_blocking(move || {
            store.count_entries_where(&prefixes, &|k, v| purge_matches(&scope, &ctx, k, v))
        })
        .await
        .map_err(|e| StoreError::Write(format!("purge task: {e}")))?;
    }
    store
        .delete_entries_where(
            prefixes,
            Arc::new(move |k: &str, v: &[u8]| purge_matches(&scope, &ctx, k, v)),
        )
        .await
}

/// One exported entry (a JSON line).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRecord {
    pub key: String,
    /// Unix seconds and sub-second nanos of the stored `created_at`.
    pub created_at: u64,
    #[serde(default)]
    pub created_at_nanos: u32,
    pub verified_at: u64,
    pub status: Status,
    pub source: EntrySource,
    /// `views` bit flags: 1 JSON, 2 GEO, 4 BATCH.
    pub views: u8,
    /// The upstream `addressMatches` / `geographies` JSON, byte-exact.
    pub json: Option<Box<RawValue>>,
    pub batch: Option<BatchView>,
    pub benchmark_name: String,
    pub vintage_name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
    #[error("{0}")]
    Invalid(String),
}

pub fn export_record(key: &str, bytes: &[u8], codec: &Codec) -> Result<ExportRecord, RecordError> {
    let (envelope, created_at) = decode_stored(bytes)?;
    let e = Entry::decode(&envelope, codec)?;
    let since = created_at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let json = match e.payload.json {
        Some(b) => {
            let text = String::from_utf8(b.into_vec())
                .map_err(|_| RecordError::Invalid("stored JSON is not UTF-8".into()))?;
            Some(RawValue::from_string(text).map_err(|e| RecordError::Invalid(e.to_string()))?)
        }
        None => None,
    };
    Ok(ExportRecord {
        key: key.to_owned(),
        created_at: since.as_secs(),
        created_at_nanos: since.subsec_nanos(),
        verified_at: e.verified_at,
        status: e.status,
        source: e.source,
        views: e.views.bits(),
        json,
        batch: e.payload.batch,
        benchmark_name: e.payload.benchmark_name,
        vintage_name: e.payload.vintage_name,
    })
}

/// The key and stored bytes for an imported record (re-encoded with `codec`).
pub fn import_record(rec: ExportRecord, codec: &Codec) -> Result<(String, Vec<u8>), RecordError> {
    CacheKey::parse(&rec.key).map_err(|e| RecordError::Invalid(e.to_string()))?;
    if rec.views > 0x07 {
        return Err(RecordError::Invalid(format!("invalid views {}", rec.views)));
    }
    let entry = Entry {
        status: rec.status,
        source: rec.source,
        views: Views::from_bits(rec.views),
        verified_at: rec.verified_at,
        payload: EntryPayload {
            json: rec.json.map(|j| ByteBuf::from(j.get().as_bytes().to_vec())),
            batch: rec.batch,
            benchmark_name: rec.benchmark_name,
            vintage_name: rec.vintage_name,
        },
    };
    let created_at =
        UNIX_EPOCH + Duration::new(rec.created_at, rec.created_at_nanos.min(999_999_999));
    Ok((rec.key, encode_stored(&entry.encode(codec)?, created_at)))
}

/// Keep `existing` when it is newer than the imported entry (an import never rolls an
/// entry back).
pub fn import_keeps_existing(existing: &[u8], imported: &[u8]) -> bool {
    match (decode_stored(existing), decode_stored(imported)) {
        (Ok((_, old)), Ok((_, new))) => old > new,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cache::key::KeyInput, config::Config};

    fn stored(status: Status, source: EntrySource, created_at: SystemTime, json: &str) -> Vec<u8> {
        let e = Entry {
            status,
            source,
            views: Views::JSON,
            verified_at: 7,
            payload: EntryPayload {
                json: Some(ByteBuf::from(json.as_bytes().to_vec())),
                batch: None,
                benchmark_name: "Public_AR_Current".into(),
                vintage_name: "Current_Current".into(),
            },
        };
        encode_stored(&e.encode(Codec::embedded()).unwrap(), created_at)
    }

    fn key(b: u32, v: u32) -> String {
        CacheKey::new(
            b,
            v,
            &KeyInput::Oneline {
                address: "1 A St".into(),
            },
        )
        .to_string()
    }

    #[test]
    fn scope_predicates() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000 * 86_400);
        let ctx = PurgeCtx {
            ttl: Config::default().ttl,
            classes: HashMap::from([(2020, BenchmarkClass::Fixed)]),
            now,
        };
        let old = now - Duration::from_secs(200 * 86_400);
        let fresh_match = stored(Status::Match, EntrySource::Single, now, "[1]");
        let old_match = stored(Status::Match, EntrySource::Single, old, "[1]");
        let old_nomatch = stored(Status::NoMatch, EntrySource::Batch, old, "[]");

        let expired = PurgeScope::Expired;
        assert!(!purge_matches(&expired, &ctx, &key(4, 4), &fresh_match));
        assert!(purge_matches(&expired, &ctx, &key(4, 4), &old_match));
        // Fixed-class Match never expires.
        assert!(!purge_matches(&expired, &ctx, &key(2020, 2020), &old_match));
        assert!(purge_matches(&expired, &ctx, &key(4, 4), b"garbage"));

        let nm = PurgeScope::Status(Status::NoMatch);
        assert!(purge_matches(&nm, &ctx, &key(4, 4), &old_nomatch));
        assert!(!purge_matches(&nm, &ctx, &key(4, 4), &old_match));
        assert!(!purge_matches(&nm, &ctx, &key(4, 4), b"garbage"));

        assert!(purge_matches(
            &PurgeScope::Benchmark(4),
            &ctx,
            &key(4, 8),
            b""
        ));
        assert!(!purge_matches(
            &PurgeScope::Benchmark(4),
            &ctx,
            &key(8, 4),
            b""
        ));
        let v = PurgeScope::Vintage {
            benchmark_id: 4,
            vintage_id: 8,
        };
        assert!(purge_matches(&v, &ctx, &key(4, 8), b""));
        assert!(!purge_matches(&v, &ctx, &key(4, 4), b""));
        assert_eq!(PurgeScope::Benchmark(4).prefixes().len(), 4);
        assert_eq!(PurgeScope::All.prefixes(), Vec::<String>::new());
    }

    #[test]
    fn export_import_round_trip_is_exact() {
        let created = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
        let json = r#"[{"coordinates":{"x":-76.92836638093,"y":38.84505589808}}]"#;
        let bytes = stored(Status::Match, EntrySource::Single, created, json);
        let rec = export_record(&key(4, 4), &bytes, Codec::embedded()).unwrap();
        let line = serde_json::to_string(&rec).unwrap();
        assert!(line.contains(json), "{line}");
        let back: ExportRecord = serde_json::from_str(&line).unwrap();
        let (k, restored) = import_record(back, Codec::embedded()).unwrap();
        assert_eq!(k, key(4, 4));
        let (env_a, at_a) = decode_stored(&bytes).unwrap();
        let (env_b, at_b) = decode_stored(&restored).unwrap();
        assert_eq!(at_a, at_b);
        let (a, b) = (
            Entry::decode(&env_a, Codec::embedded()).unwrap(),
            Entry::decode(&env_b, Codec::embedded()).unwrap(),
        );
        assert_eq!(a, b);
        assert!(!import_keeps_existing(&bytes, &restored));
        let older = stored(
            Status::Match,
            EntrySource::Single,
            created - Duration::from_secs(1),
            json,
        );
        assert!(import_keeps_existing(&bytes, &older));
    }
}
