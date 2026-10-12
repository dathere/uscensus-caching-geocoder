//! The store file must stay readable by `cached::RedbCache` (and so by qsv tooling).

use std::time::{Duration, SystemTime};

use cached::{ConcurrentCached, RedbCache};
use redb::{Database, ReadableDatabase, TableDefinition};
use ucgeocoder::cache::{
    codec::{Codec, CodecKind},
    envelope::{Entry, EntrySource, Header, UpstreamSingle, decode_stored, encode_stored},
    key::{CacheKey, KeyInput},
};

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("cached_disk_cache");

fn entry() -> Entry {
    let body = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/live/geo_address_all.json"
    ))
    .unwrap();
    let s = UpstreamSingle::from_response(&body).unwrap();
    Entry {
        status: s.status,
        source: EntrySource::Single,
        views: s.views,
        verified_at: 1_760_000_000,
        payload: s.payload,
    }
}

fn key() -> CacheKey {
    CacheKey::new(
        4,
        4,
        &KeyInput::Address {
            street: "4600 Silver Hill Rd".into(),
            city: "Washington".into(),
            state: "DC".into(),
            zip: String::new(),
        },
    )
}

#[test]
fn our_entries_are_readable_by_cached_redb_cache() {
    let dir = tempfile::tempdir().unwrap();
    let codec = Codec::embedded();
    let e = entry();
    let envelope = e.encode(codec).unwrap();
    assert_eq!(Header::parse(&envelope).unwrap().codec, CodecKind::ZstdDict);
    let created_at = SystemTime::now() - Duration::from_secs(60);

    {
        let db = Database::create(dir.path().join("geocode-census_v3.redb")).unwrap();
        let w = db.begin_write().unwrap();
        {
            let mut t = w.open_table(TABLE).unwrap();
            t.insert(
                key().as_str(),
                encode_stored(&envelope, created_at).as_slice(),
            )
            .unwrap();
        }
        w.commit().unwrap();
    }

    let cache: RedbCache<String, Vec<u8>> = RedbCache::builder("geocode-census")
        .disk_dir(dir.path())
        .build()
        .unwrap();
    let got = cache
        .cache_get(&key().to_string())
        .unwrap()
        .expect("entry visible to cached");
    assert_eq!(got, envelope);
    assert_eq!(Entry::decode(&got, codec).unwrap(), e);
}

#[test]
fn entries_written_by_cached_are_readable_by_us() {
    let dir = tempfile::tempdir().unwrap();
    let codec = Codec::embedded();
    let e = entry();
    let envelope = e.encode(codec).unwrap();

    {
        let cache: RedbCache<String, Vec<u8>> = RedbCache::builder("geocode-census")
            .disk_dir(dir.path())
            .build()
            .unwrap();
        cache
            .cache_set(key().to_string(), envelope.clone())
            .unwrap();
    }

    let db = Database::open(dir.path().join("geocode-census_v3.redb")).unwrap();
    let r = db.begin_read().unwrap();
    let t = r.open_table(TABLE).unwrap();
    let raw = t.get(key().as_str()).unwrap().unwrap().value().to_vec();
    let (env, created_at) = decode_stored(&raw).unwrap();
    assert_eq!(env, envelope);
    assert!(SystemTime::now().duration_since(created_at).unwrap() < Duration::from_secs(60));
    assert_eq!(Entry::decode(&env, codec).unwrap(), e);
}
