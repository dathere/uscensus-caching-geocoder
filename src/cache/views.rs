//! Can a stored entry answer a request? (PLAN.md §5.4)

use std::time::SystemTime;

use super::{
    codec::Codec,
    envelope::{BatchView, Entry, EnvelopeError, Status, Views, decode_stored, encode_stored},
    policy::{BenchmarkClass, Freshness, freshness, ttl},
};
use crate::{
    census::batch_csv::{RowResult, heuristic_from_geographies, heuristic_from_matches},
    config::{Config, TtlConfig},
};

/// The response shape a single-record request needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// `locations/*`: `addressMatches` without geographies.
    Locations,
    /// `geographies/{address search}`: `addressMatches` with geographies.
    Geographies,
    /// `geographies/coordinates`: the geographies object.
    Coordinates,
}

/// Whether `entry` can serve `need` without going upstream.
///
/// A NoMatch/Tie that came from a single-record answer (it has the JSON view) answers
/// any address view: the response is `addressMatches: []` whatever was asked (a NoMatch
/// fetched with `layers=all` has no match to carry the GEO view on). A NoMatch/Tie known
/// only from a batch does not: Census's batch matching is load-dependent, so single
/// lookups ask upstream again (and upgrade the entry). Coordinates always need the
/// stored geographies.
pub fn satisfies(entry: &Entry, need: Need) -> bool {
    let v = entry.views;
    match (need, entry.status) {
        (Need::Coordinates, _) => v.contains(Views::GEO),
        (_, Status::NoMatch | Status::Tie) => v.contains(Views::JSON),
        (Need::Locations, Status::Match) => v.contains(Views::JSON),
        (Need::Geographies, Status::Match) => v.contains(Views::JSON | Views::GEO),
    }
}

/// A decoded stored entry with its age.
#[derive(Debug, Clone)]
pub struct Stored {
    pub entry: Entry,
    pub created_at: SystemTime,
}

#[derive(Debug, Clone)]
pub enum Lookup {
    /// Fresh and sufficient: serve it.
    Hit(Stored),
    /// Usable only if the upstream refetch fails (`X-Cache: STALE`).
    Stale(Stored),
    /// Fetch upstream. Carries any existing entry so its batch view can be merged.
    Miss(Option<Stored>),
    /// The stored bytes cannot be decoded (corrupt or unknown dictionary): delete them.
    Corrupt,
}

pub fn decode(bytes: &[u8], codec: &Codec) -> Result<Stored, EnvelopeError> {
    let (envelope, created_at) = decode_stored(bytes)?;
    Ok(Stored {
        entry: Entry::decode(&envelope, codec)?,
        created_at,
    })
}

pub fn classify(
    bytes: Option<&[u8]>,
    codec: &Codec,
    cfg: &Config,
    class: BenchmarkClass,
    need: Need,
    now: SystemTime,
) -> Lookup {
    let Some(bytes) = bytes else {
        return Lookup::Miss(None);
    };
    let Ok(stored) = decode(bytes, codec) else {
        return Lookup::Corrupt;
    };
    if !satisfies(&stored.entry, need) {
        return Lookup::Miss(Some(stored));
    }
    let e = &stored.entry;
    let life = ttl(&cfg.ttl, e.status, e.source, class);
    match freshness(stored.created_at, now, life, cfg.cache.serve_stale_on_error) {
        Freshness::Fresh => Lookup::Hit(stored),
        Freshness::Stale => Lookup::Stale(stored),
        Freshness::Expired => Lookup::Miss(Some(stored)),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::cache::envelope::{EntryPayload, EntrySource, encode_stored};

    fn entry(status: Status, views: Views) -> Entry {
        Entry {
            status,
            source: EntrySource::Single,
            views,
            verified_at: 0,
            payload: EntryPayload::default(),
        }
    }

    #[test]
    fn view_table() {
        let full = entry(Status::Match, Views::JSON | Views::GEO);
        let json_only = entry(Status::Match, Views::JSON);
        let batch_only = entry(Status::Match, Views::BATCH);
        let nomatch_json = entry(Status::NoMatch, Views::JSON);
        let nomatch_batch = entry(Status::NoMatch, Views::BATCH);
        let coords = entry(Status::Match, Views::GEO);

        assert!(satisfies(&full, Need::Locations) && satisfies(&full, Need::Geographies));
        assert!(satisfies(&json_only, Need::Locations));
        assert!(!satisfies(&json_only, Need::Geographies));
        assert!(!satisfies(&batch_only, Need::Locations));
        assert!(satisfies(&nomatch_json, Need::Geographies));
        assert!(
            !satisfies(&nomatch_batch, Need::Locations),
            "a batch-only NoMatch must not answer single lookups"
        );
        assert!(!satisfies(
            &entry(Status::Tie, Views::BATCH),
            Need::Geographies
        ));
        assert!(satisfies(&coords, Need::Coordinates));
        assert!(!satisfies(&nomatch_batch, Need::Coordinates));
    }

    #[test]
    fn classify_freshness() {
        let codec = Codec::default();
        let mut cfg = Config::default();
        let e = entry(Status::Match, Views::JSON | Views::GEO);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let stored =
            |age: u64| encode_stored(&e.encode(&codec).unwrap(), now - Duration::from_secs(age));
        let day = 86_400;

        assert!(matches!(
            classify(
                Some(&stored(day)),
                &codec,
                &cfg,
                BenchmarkClass::Current,
                Need::Locations,
                now
            ),
            Lookup::Hit(_)
        ));
        assert!(matches!(
            classify(
                Some(&stored(200 * day)),
                &codec,
                &cfg,
                BenchmarkClass::Current,
                Need::Locations,
                now
            ),
            Lookup::Stale(_)
        ));
        assert!(matches!(
            classify(
                Some(&stored(200 * day)),
                &codec,
                &cfg,
                BenchmarkClass::Fixed,
                Need::Locations,
                now
            ),
            Lookup::Hit(_)
        ));
        cfg.cache.serve_stale_on_error = false;
        assert!(matches!(
            classify(
                Some(&stored(200 * day)),
                &codec,
                &cfg,
                BenchmarkClass::Current,
                Need::Locations,
                now
            ),
            Lookup::Miss(Some(_))
        ));
        assert!(matches!(
            classify(
                None,
                &codec,
                &cfg,
                BenchmarkClass::Current,
                Need::Locations,
                now
            ),
            Lookup::Miss(None)
        ));
        assert!(matches!(
            classify(
                Some(b"garbage"),
                &codec,
                &cfg,
                BenchmarkClass::Current,
                Need::Locations,
                now
            ),
            Lookup::Corrupt
        ));
    }
}

// ---------------------------------------------------------------- batch rows

/// What a batch row needs from an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchNeed {
    /// `locations/addressbatch` columns.
    Locations,
    /// `geographies/addressbatch`: also state/county/tract/block.
    Geographies,
    /// `geographies/coordinatesbatch`.
    Coordinates,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchLookup {
    /// `heuristic`: synthesized from single-record JSON (match type guessed).
    Hit {
        result: RowResult,
        heuristic: bool,
    },
    Miss,
}

/// Can a stored entry answer one batch row? Only fresh entries count (no stale
/// fallback for batch). `echo` is the row's `street, city, state, zip` input.
pub fn classify_batch(
    bytes: Option<&[u8]>,
    codec: &Codec,
    cfg: &Config,
    class: BenchmarkClass,
    need: BatchNeed,
    now: SystemTime,
    echo: &str,
) -> BatchLookup {
    let Some(stored) = bytes.and_then(|b| decode(b, codec).ok()) else {
        return BatchLookup::Miss;
    };
    let e = &stored.entry;
    if freshness(
        stored.created_at,
        now,
        ttl(&cfg.ttl, e.status, e.source, class),
        false,
    ) != Freshness::Fresh
    {
        return BatchLookup::Miss;
    }
    let hit = |result| BatchLookup::Hit {
        result,
        heuristic: false,
    };

    if e.views.contains(Views::BATCH)
        && let Some(v) = &e.payload.batch
    {
        match e.status {
            Status::NoMatch => return hit(RowResult::NoMatch),
            Status::Tie => return hit(RowResult::Tie),
            Status::Match => {
                let complete = match need {
                    BatchNeed::Locations => v.matched_address.is_some(),
                    BatchNeed::Geographies => v.matched_address.is_some() && v.state.is_some(),
                    BatchNeed::Coordinates => v.state.is_some(),
                };
                if complete {
                    return hit(RowResult::Match(Box::new(v.clone())));
                }
            }
        }
    }

    let Some(json) = e.payload.json.as_deref() else {
        return BatchLookup::Miss;
    };
    let heuristic = |v: Option<BatchView>| match v {
        Some(v) => BatchLookup::Hit {
            result: RowResult::Match(Box::new(v)),
            heuristic: true,
        },
        None => BatchLookup::Miss,
    };
    match (need, e.status) {
        (BatchNeed::Coordinates, Status::NoMatch) if e.views.contains(Views::GEO) => {
            hit(RowResult::NoMatch)
        }
        (BatchNeed::Coordinates, Status::Match) if e.views.contains(Views::GEO) => {
            heuristic(heuristic_from_geographies(json))
        }
        // A single-record NoMatch is the reliable kind; batch rows may reuse it.
        (BatchNeed::Locations | BatchNeed::Geographies, Status::NoMatch)
            if e.views.contains(Views::JSON) =>
        {
            hit(RowResult::NoMatch)
        }
        (BatchNeed::Locations, Status::Match) if e.views.contains(Views::JSON) => {
            heuristic(heuristic_from_matches(json, echo, false))
        }
        (BatchNeed::Geographies, Status::Match) if e.views.contains(Views::JSON | Views::GEO) => {
            heuristic(heuristic_from_matches(json, echo, true))
        }
        _ => BatchLookup::Miss,
    }
}

/// Decide, inside the write transaction, what a batch result writes over `existing`.
///
/// A fresh Match is never replaced by a batch No_Match/Tie (batch negatives are
/// load-dependent). A batch Match only adds its batch view to a fresh single-record
/// Match (one carrying `JSON` or `GEO`), and never replaces a fresh geography-complete
/// batch Match with one lacking geography columns. Anything else (no entry, expired,
/// or a negative) is replaced by the batch entry.
pub fn merge_batch(
    existing: Option<&[u8]>,
    new: &Entry,
    codec: &Codec,
    ttl_cfg: &TtlConfig,
    class: BenchmarkClass,
    now: SystemTime,
) -> Option<Vec<u8>> {
    let encode_new = || new.encode(codec).ok().map(|env| encode_stored(&env, now));
    let Some(old) = existing.and_then(|b| decode(b, codec).ok()) else {
        return encode_new();
    };
    let o = &old.entry;
    let fresh = freshness(
        old.created_at,
        now,
        ttl(ttl_cfg, o.status, o.source, class),
        false,
    ) == Freshness::Fresh;
    if !(fresh && o.status == Status::Match) {
        return encode_new();
    }
    if new.status != Status::Match {
        return None;
    }
    // Never trade a geography-complete batch view for a locations-only one, whether
    // the entry is batch-only or a single-record entry carrying a batch view.
    let has_geo = |e: &Entry| e.payload.batch.as_ref().is_some_and(|b| b.state.is_some());
    if has_geo(o) && !has_geo(new) {
        return None;
    }
    let single = o.views.contains(Views::JSON) || o.views.contains(Views::GEO);
    if single {
        let mut merged = o.clone();
        merged.payload.batch.clone_from(&new.payload.batch);
        merged.views = merged.views | Views::BATCH;
        return merged
            .encode(codec)
            .ok()
            .map(|env| encode_stored(&env, old.created_at));
    }
    encode_new()
}

#[cfg(test)]
mod batch_tests {
    use std::time::Duration;

    use super::*;
    use crate::cache::envelope::{EntryPayload, EntrySource, UpstreamSingle};

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/live/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn single_entry() -> Entry {
        let s = UpstreamSingle::from_response(&fixture("geo_address_all.json")).unwrap();
        Entry {
            status: s.status,
            source: EntrySource::Single,
            views: s.views,
            verified_at: 0,
            payload: s.payload,
        }
    }

    fn batch_entry(status: Status, view: BatchView) -> Entry {
        Entry {
            status,
            source: EntrySource::Batch,
            views: Views::BATCH,
            verified_at: 0,
            payload: EntryPayload {
                batch: Some(view),
                ..EntryPayload::default()
            },
        }
    }

    const ECHO: &str = "4600 Silver Hill Rd, Washington, DC, 20233";

    #[test]
    fn batch_classification() {
        let codec = Codec::default();
        let cfg = Config::default();
        let now = SystemTime::now();
        let st = |e: &Entry| encode_stored(&e.encode(&codec).unwrap(), now);
        let c = |b: &[u8], need| {
            classify_batch(
                Some(b),
                &codec,
                &cfg,
                BenchmarkClass::Current,
                need,
                now,
                ECHO,
            )
        };

        let single = st(&single_entry());
        assert!(matches!(
            c(&single, BatchNeed::Geographies),
            BatchLookup::Hit {
                heuristic: true,
                ..
            }
        ));

        let loc_only = st(&batch_entry(
            Status::Match,
            BatchView {
                matched_address: Some("X".into()),
                ..BatchView::default()
            },
        ));
        assert!(matches!(
            c(&loc_only, BatchNeed::Locations),
            BatchLookup::Hit {
                heuristic: false,
                ..
            }
        ));
        assert_eq!(
            c(&loc_only, BatchNeed::Geographies),
            BatchLookup::Miss,
            "no geography columns"
        );

        let nm = st(&batch_entry(Status::NoMatch, BatchView::default()));
        assert_eq!(
            c(&nm, BatchNeed::Locations),
            BatchLookup::Hit {
                result: RowResult::NoMatch,
                heuristic: false
            }
        );
        let tie = st(&batch_entry(Status::Tie, BatchView::default()));
        assert!(matches!(
            c(&tie, BatchNeed::Geographies),
            BatchLookup::Hit {
                result: RowResult::Tie,
                ..
            }
        ));

        let old = encode_stored(
            &single_entry().encode(&codec).unwrap(),
            now - Duration::from_secs(400 * 86_400),
        );
        assert_eq!(
            c(&old, BatchNeed::Locations),
            BatchLookup::Miss,
            "expired entries are misses"
        );
    }

    #[test]
    fn merge_rules() {
        let codec = Codec::default();
        let ttl_cfg = Config::default().ttl;
        let now = SystemTime::now();
        let single = encode_stored(
            &single_entry().encode(&codec).unwrap(),
            now - Duration::from_secs(10),
        );
        let m = |existing: Option<&[u8]>, new: &Entry| {
            merge_batch(
                existing,
                new,
                &codec,
                &ttl_cfg,
                BenchmarkClass::Current,
                now,
            )
        };

        let nomatch = batch_entry(Status::NoMatch, BatchView::default());
        assert_eq!(
            m(Some(&single), &nomatch),
            None,
            "batch No_Match never downgrades a single Match"
        );

        let matched = batch_entry(
            Status::Match,
            BatchView {
                matched_address: Some("A".into()),
                ..BatchView::default()
            },
        );
        let merged = decode(&m(Some(&single), &matched).unwrap(), &codec).unwrap();
        assert!(
            merged
                .entry
                .views
                .contains(Views::JSON | Views::GEO | Views::BATCH)
        );
        assert_eq!(merged.entry.source, EntrySource::Single);
        assert!(merged.entry.payload.json.is_some());
        assert_eq!(
            merged.created_at,
            now - Duration::from_secs(10),
            "keeps the single entry's age"
        );

        let fresh_write = decode(&m(None, &nomatch).unwrap(), &codec).unwrap();
        assert_eq!(fresh_write.entry.status, Status::NoMatch);
        let old_batch = encode_stored(&nomatch.encode(&codec).unwrap(), now);
        let replaced = decode(&m(Some(&old_batch), &matched).unwrap(), &codec).unwrap();
        assert_eq!(replaced.entry.status, Status::Match);
    }

    #[test]
    fn merge_protects_coordinate_singles_and_geography_complete_batches() {
        let codec = Codec::default();
        let ttl_cfg = Config::default().ttl;
        let now = SystemTime::now();
        let m = |existing: &[u8], new: &Entry| {
            merge_batch(
                Some(existing),
                new,
                &codec,
                &ttl_cfg,
                BenchmarkClass::Current,
                now,
            )
        };

        // Single-record coordinates entries only carry GEO.
        let s = UpstreamSingle::from_response(&fixture("geo_coords_all.json")).unwrap();
        let coords = Entry {
            status: s.status,
            source: EntrySource::Single,
            views: s.views,
            verified_at: 0,
            payload: s.payload,
        };
        let coords = encode_stored(&coords.encode(&codec).unwrap(), now);
        assert_eq!(
            m(&coords, &batch_entry(Status::NoMatch, BatchView::default())),
            None
        );
        let geo_view = BatchView {
            state: Some("24".into()),
            ..BatchView::default()
        };
        let merged = decode(
            &m(&coords, &batch_entry(Status::Match, geo_view.clone())).unwrap(),
            &codec,
        )
        .unwrap();
        assert!(merged.entry.views.contains(Views::GEO | Views::BATCH));

        // A geography-complete batch Match is not replaced by a locations-only one,
        // in either publication order.
        let loc_view = BatchView {
            matched_address: Some("A".into()),
            ..BatchView::default()
        };
        let geo_batch = encode_stored(
            &batch_entry(Status::Match, geo_view.clone())
                .encode(&codec)
                .unwrap(),
            now,
        );
        let loc_batch = encode_stored(
            &batch_entry(Status::Match, loc_view.clone())
                .encode(&codec)
                .unwrap(),
            now,
        );
        assert_eq!(m(&geo_batch, &batch_entry(Status::Match, loc_view)), None);
        let upgraded = decode(
            &m(&loc_batch, &batch_entry(Status::Match, geo_view)).unwrap(),
            &codec,
        )
        .unwrap();
        assert!(upgraded.entry.payload.batch.unwrap().state.is_some());
    }

    #[test]
    fn single_entry_keeps_its_geography_complete_batch_view() {
        let codec = Codec::default();
        let ttl_cfg = Config::default().ttl;
        let now = SystemTime::now();
        // A JSON-only single Match that already gained a geography-complete batch view.
        let mut single = single_entry();
        single.views = Views::JSON | Views::BATCH;
        single.payload.batch = Some(BatchView {
            matched_address: Some("A".into()),
            state: Some("24".into()),
            ..BatchView::default()
        });
        let stored = encode_stored(&single.encode(&codec).unwrap(), now);
        let loc_only = batch_entry(
            Status::Match,
            BatchView {
                matched_address: Some("A".into()),
                ..BatchView::default()
            },
        );
        assert_eq!(
            merge_batch(
                Some(&stored),
                &loc_only,
                &codec,
                &ttl_cfg,
                BenchmarkClass::Current,
                now
            ),
            None
        );
    }
}
