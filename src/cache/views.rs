//! Can a stored entry answer a request? (PLAN.md §5.4)

use std::time::SystemTime;

use super::{
    codec::Codec,
    envelope::{Entry, EnvelopeError, Status, Views, decode_stored},
    policy::{BenchmarkClass, Freshness, freshness, ttl},
};
use crate::config::Config;

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
