//! Cache entries (`cached_disk_cache`). Values are the cached-compatible stored bytes
//! from [`crate::cache::envelope::encode_stored`]; decoding is the cache layer's job.

use std::{
    ops::Bound,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

use redb::{ReadableTable, ReadableTableMetadata};

use super::{Store, StoreError, tables::ENTRIES};
use crate::cache::envelope::{Header, Views, decode_stored};

/// Result of a conditional put.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    Written,
    /// A newer entry with the required views was already there; nothing written.
    KeptExisting,
}

impl Store {
    pub fn get_entry(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.read(|txn| {
            let t = txn.open_table(ENTRIES)?;
            Ok(t.get(key)?.map(|g| g.value().to_vec()))
        })
    }

    /// Look up many keys in one read transaction (batch hit/miss partitioning).
    pub fn get_entries(&self, keys: &[&str]) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        self.read(|txn| {
            let t = txn.open_table(ENTRIES)?;
            keys.iter()
                .map(|k| Ok(t.get(*k)?.map(|g| g.value().to_vec())))
                .collect()
        })
    }

    /// Up to `limit` entries whose key starts with `prefix`, in key order, starting
    /// strictly after `after` (a pagination cursor) when given.
    pub fn scan_entries(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
        self.read(|txn| {
            let t = txn.open_table(ENTRIES)?;
            let start = match after {
                Some(a) if a >= prefix => Bound::Excluded(a),
                _ => Bound::Included(prefix),
            };
            let mut out = Vec::new();
            for item in t.range::<&str>((start, Bound::Unbounded))? {
                if out.len() >= limit {
                    break;
                }
                let (k, v) = item?;
                let k = k.value();
                if !k.starts_with(prefix) {
                    break;
                }
                out.push((k.to_owned(), v.value().to_vec()));
            }
            Ok(out)
        })
    }

    pub fn entry_count(&self) -> Result<u64, StoreError> {
        self.read(|txn| Ok(txn.open_table(ENTRIES)?.len()?))
    }

    pub async fn put_entry(&self, key: String, stored: Vec<u8>) -> Result<(), StoreError> {
        self.write(Box::new(move |ctx| {
            ctx.entries()?.insert(key.as_str(), stored.as_slice())?;
            Ok(())
        }))
        .await
    }

    /// Insert many entries in one op (one transaction).
    pub async fn put_entries(&self, items: Vec<(String, Vec<u8>)>) -> Result<(), StoreError> {
        self.write(Box::new(move |ctx| {
            let t = ctx.entries()?;
            for (k, v) in &items {
                t.insert(k.as_str(), v.as_slice())?;
            }
            Ok(())
        }))
        .await
    }

    /// Insert `stored` unless, at commit time, the key already holds an entry that has
    /// every view in `keep_views` and was created at or after `not_before`. The check
    /// runs inside the write transaction, so it is atomic with respect to every other
    /// write (including ones queued in the same batch).
    pub async fn put_entry_unless_newer(
        &self,
        key: String,
        stored: Vec<u8>,
        keep_views: Views,
        not_before: SystemTime,
    ) -> Result<PutOutcome, StoreError> {
        let kept = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&kept);
        self.write(Box::new(move |ctx| {
            let t = ctx.entries()?;
            let existing = t.get(key.as_str())?.map(|g| g.value().to_vec());
            let keep = existing.is_some_and(|bytes| {
                decode_stored(&bytes).is_ok_and(|(envelope, created_at)| {
                    created_at >= not_before
                        && Header::parse(&envelope).is_ok_and(|h| h.views.contains(keep_views))
                })
            });
            // Ops may be re-run after a failed batch: always set the flag.
            flag.store(keep, Ordering::SeqCst);
            if !keep {
                t.insert(key.as_str(), stored.as_slice())?;
            }
            Ok(())
        }))
        .await?;
        Ok(if kept.load(Ordering::SeqCst) {
            PutOutcome::KeptExisting
        } else {
            PutOutcome::Written
        })
    }

    pub async fn delete_entry(&self, key: String) -> Result<(), StoreError> {
        self.write(Box::new(move |ctx| {
            ctx.entries()?.remove(key.as_str())?;
            Ok(())
        }))
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::test_util::opts;
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_put_get() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&opts(dir.path())).unwrap());
        let mut tasks = Vec::new();
        for t in 0..8 {
            let store = Arc::clone(&store);
            tasks.push(tokio::spawn(async move {
                for i in 0..50 {
                    let key = format!("v1|address|4|4|{t:02}-{i:03}");
                    let val = format!("value {t} {i}").into_bytes();
                    store.put_entry(key.clone(), val.clone()).await.unwrap();
                    // Read-your-write: the put is committed once it returns.
                    assert_eq!(store.get_entry(&key).unwrap(), Some(val));
                    // Interleave reads of other tasks' keys.
                    let _ = store
                        .get_entry(&format!("v1|address|4|4|{:02}-{i:03}", (t + 1) % 8))
                        .unwrap();
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(store.entry_count().unwrap(), 400);
        let keys: Vec<String> = (0..8)
            .map(|t| format!("v1|address|4|4|{t:02}-049"))
            .collect();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let got = store.get_entries(&refs).unwrap();
        assert!(got.iter().all(Option::is_some));
        assert_eq!(store.get_entries(&["nope"]).unwrap(), vec![None]);
    }

    #[tokio::test]
    async fn prefix_scan_is_ordered_and_paginates() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&opts(dir.path())).unwrap();
        let keys = [
            "v1|address|4|4|C",
            "v1|address|42|4|A",
            "v1|address|4|4|A",
            "v1|oneline|4|4|A",
            "v1|address|4|426|B",
            "v1|address|4|4|B",
            "v1|address|40|4|A",
        ];
        store
            .put_entries(
                keys.iter()
                    .map(|k| ((*k).to_owned(), k.as_bytes().to_vec()))
                    .collect(),
            )
            .await
            .unwrap();

        let all: Vec<String> = store
            .scan_entries("v1|address|4|", None, 100)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            all,
            [
                "v1|address|4|426|B",
                "v1|address|4|4|A",
                "v1|address|4|4|B",
                "v1|address|4|4|C"
            ]
        );

        let page1 = store.scan_entries("v1|address|4|", None, 2).unwrap();
        assert_eq!(page1.len(), 2);
        let cursor = page1.last().unwrap().0.clone();
        let page2: Vec<String> = store
            .scan_entries("v1|address|4|", Some(&cursor), 2)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(page2, ["v1|address|4|4|B", "v1|address|4|4|C"]);
        assert!(
            store
                .scan_entries("v1|address|4|", Some("v1|address|4|4|C"), 2)
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.scan_entries("", None, 100).unwrap().len(), keys.len());
        assert_eq!(store.scan_entries("v1|zzz", None, 100).unwrap().len(), 0);
    }

    #[tokio::test]
    async fn restart_persistence_without_explicit_flush() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(&opts(dir.path())).unwrap();
            store.put_entry("k1".into(), b"one".to_vec()).await.unwrap();
            store.put_entry("k2".into(), b"two".to_vec()).await.unwrap();
            store.delete_entry("k2".into()).await.unwrap();
            // Dropping the store must drain the writer and commit durably.
        }
        let store = Store::open(&opts(dir.path())).unwrap();
        assert_eq!(store.get_entry("k1").unwrap(), Some(b"one".to_vec()));
        assert_eq!(store.get_entry("k2").unwrap(), None);
        assert_eq!(store.entry_count().unwrap(), 1);
    }

    #[tokio::test]
    async fn compact_reclaims_space() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&opts(dir.path())).unwrap();
        let blob = vec![7u8; 8 * 1024];
        let items: Vec<(String, Vec<u8>)> = (0..400)
            .map(|i| (format!("key{i:04}"), blob.clone()))
            .collect();
        store.put_entries(items).await.unwrap();
        store.flush().await.unwrap();
        for i in 0..400 {
            if i % 20 != 0 {
                store.delete_entry(format!("key{i:04}")).await.unwrap();
            }
        }
        store.flush().await.unwrap();
        let before = store.file_size().unwrap();
        let s = store.clone();
        let compacted = tokio::task::spawn_blocking(move || s.compact())
            .await
            .unwrap()
            .unwrap();
        assert!(compacted);
        let after = store.file_size().unwrap();
        assert!(after < before, "{after} >= {before}");
        assert_eq!(store.entry_count().unwrap(), 20);
        assert_eq!(store.get_entry("key0020").unwrap(), Some(blob));
    }

    fn stored_with(views: crate::cache::envelope::Views, created_at: SystemTime) -> Vec<u8> {
        use crate::cache::{
            codec::Codec,
            envelope::{Entry, EntryPayload, EntrySource, Status, encode_stored},
        };
        let e = Entry {
            status: Status::Match,
            source: EntrySource::Single,
            views,
            verified_at: 0,
            payload: EntryPayload::default(),
        };
        encode_stored(&e.encode(&Codec::default()).unwrap(), created_at)
    }

    fn views_of(store: &Store, key: &str) -> crate::cache::envelope::Views {
        let (env, _) = decode_stored(&store.get_entry(key).unwrap().unwrap()).unwrap();
        Header::parse(&env).unwrap().views
    }

    #[tokio::test]
    async fn conditional_put_is_atomic_within_one_batch_window() {
        use crate::cache::envelope::Views;
        let dir = tempfile::tempdir().unwrap();
        let mut o = opts(dir.path());
        o.writer.flush = std::time::Duration::from_millis(50);
        let store = Store::open(&o).unwrap();
        let fetch_started = SystemTime::now();
        let geo = stored_with(Views::JSON | Views::GEO, SystemTime::now());
        let loc = stored_with(Views::JSON, SystemTime::now());

        // Geographies write queued first, locations-only second, same transaction.
        let (a, b) = tokio::join!(
            store.put_entry("k".into(), geo.clone()),
            store.put_entry_unless_newer("k".into(), loc.clone(), Views::GEO, fetch_started),
        );
        a.unwrap();
        assert_eq!(b.unwrap(), PutOutcome::KeptExisting);
        assert!(views_of(&store, "k").contains(Views::GEO));

        // Opposite order: the locations-only write lands, then geographies replaces it.
        let (b, a) = tokio::join!(
            store.put_entry_unless_newer("k2".into(), loc, Views::GEO, fetch_started),
            store.put_entry("k2".into(), geo),
        );
        assert_eq!(b.unwrap(), PutOutcome::Written);
        a.unwrap();
        assert!(views_of(&store, "k2").contains(Views::GEO));
    }

    #[tokio::test]
    async fn conditional_put_replaces_older_or_insufficient_entries() {
        use crate::cache::envelope::Views;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&opts(dir.path())).unwrap();
        let t0 = SystemTime::now();
        let old_geo = stored_with(
            Views::JSON | Views::GEO,
            t0 - std::time::Duration::from_secs(60),
        );
        store.put_entry("old".into(), old_geo).await.unwrap();
        let loc = stored_with(Views::JSON, t0);
        let r = store
            .put_entry_unless_newer("old".into(), loc.clone(), Views::GEO, t0)
            .await
            .unwrap();
        assert_eq!(
            r,
            PutOutcome::Written,
            "an entry from before this fetch began is replaced"
        );

        store
            .put_entry("json".into(), stored_with(Views::JSON, t0))
            .await
            .unwrap();
        let r = store
            .put_entry_unless_newer("json".into(), loc, Views::GEO, t0)
            .await
            .unwrap();
        assert_eq!(
            r,
            PutOutcome::Written,
            "an entry without the GEO view is replaced"
        );
    }
}
