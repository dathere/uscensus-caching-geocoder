//! Counters. Hot-path increments go to an in-memory [`StatsBuffer`]; a periodic task
//! folds them into the `ucg_stats` table so reads never write.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use redb::ReadableTable;
use tracing::warn;

use super::{Store, StoreError, tables::STATS};

/// Prefix of per-entry hit counters in `ucg_stats`.
pub const HIT_PREFIX: &str = "hit:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counter {
    Hits,
    Misses,
    StaleHits,
    UpstreamOk,
    UpstreamErr,
}

impl Counter {
    pub const ALL: [Self; 5] = [
        Self::Hits,
        Self::Misses,
        Self::StaleHits,
        Self::UpstreamOk,
        Self::UpstreamErr,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Hits => "hits",
            Self::Misses => "misses",
            Self::StaleHits => "stale_hits",
            Self::UpstreamOk => "upstream_ok",
            Self::UpstreamErr => "upstream_err",
        }
    }
}

#[derive(Debug, Default)]
pub struct StatsBuffer {
    counters: [AtomicU64; 5],
    per_key: Mutex<HashMap<String, u64>>,
}

impl StatsBuffer {
    pub fn record(&self, c: Counter) {
        self.counters[c as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Count a cache hit on `key` (also bumps [`Counter::Hits`]).
    pub fn record_hit(&self, key: &str) {
        self.record(Counter::Hits);
        *self
            .per_key
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(key.to_owned())
            .or_default() += 1;
    }

    /// Take all pending deltas as `(stats key, delta)` pairs.
    fn drain(&self) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = Counter::ALL
            .iter()
            .filter_map(|c| {
                let n = self.counters[*c as usize].swap(0, Ordering::Relaxed);
                (n > 0).then(|| (c.name().to_owned(), n))
            })
            .collect();
        let keys =
            std::mem::take(&mut *self.per_key.lock().unwrap_or_else(PoisonError::into_inner));
        out.extend(
            keys.into_iter()
                .map(|(k, n)| (format!("{HIT_PREFIX}{k}"), n)),
        );
        out
    }

    /// Put deltas back after a failed flush.
    fn restore(&self, deltas: Vec<(String, u64)>) {
        for (k, n) in deltas {
            if let Some(c) = Counter::ALL.iter().find(|c| c.name() == k) {
                self.counters[*c as usize].fetch_add(n, Ordering::Relaxed);
            } else if let Some(key) = k.strip_prefix(HIT_PREFIX) {
                *self
                    .per_key
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entry(key.to_owned())
                    .or_default() += n;
            }
        }
    }

    /// Not-yet-flushed value of a global counter.
    pub fn pending(&self, c: Counter) -> u64 {
        self.counters[c as usize].load(Ordering::Relaxed)
    }
}

impl Store {
    /// Fold pending counters into `ucg_stats`; on failure the deltas are kept.
    pub async fn flush_stats(&self, buf: &StatsBuffer) -> Result<(), StoreError> {
        let deltas = buf.drain();
        if deltas.is_empty() {
            return Ok(());
        }
        let op_deltas = deltas.clone();
        let r = self
            .write(Box::new(move |ctx| {
                let t = ctx.stats()?;
                for (k, n) in &op_deltas {
                    let cur = t.get(k.as_str())?.map_or(0, |g| g.value());
                    t.insert(k.as_str(), cur.saturating_add(*n))?;
                }
                Ok(())
            }))
            .await;
        if r.is_err() {
            buf.restore(deltas);
        }
        r
    }

    /// Persisted value of one counter (`hits`, `hit:{key}`, ...).
    pub fn stat(&self, name: &str) -> Result<u64, StoreError> {
        self.read(|txn| Ok(txn.open_table(STATS)?.get(name)?.map_or(0, |g| g.value())))
    }

    /// Persisted global counters (per-key `hit:` counters excluded).
    pub fn stat_totals(&self) -> Result<BTreeMap<String, u64>, StoreError> {
        self.read(|txn| {
            let t = txn.open_table(STATS)?;
            let mut out = BTreeMap::new();
            for c in Counter::ALL {
                out.insert(
                    c.name().to_owned(),
                    t.get(c.name())?.map_or(0, |g| g.value()),
                );
            }
            Ok(out)
        })
    }
}

/// Flush `buf` every `every` until the store's writer shuts down.
pub fn spawn_stats_flusher(
    store: Store,
    buf: Arc<StatsBuffer>,
    every: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every.max(Duration::from_secs(1)));
        tick.tick().await;
        loop {
            tick.tick().await;
            match store.flush_stats(&buf).await {
                Ok(()) => {}
                Err(StoreError::Closed) => return,
                Err(e) => warn!(error = %e, "stats flush failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_util::opts;
    use super::*;

    #[tokio::test]
    async fn stats_flush_accumulates() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&opts(dir.path())).unwrap();
        let buf = StatsBuffer::default();

        buf.record_hit("v1|address|4|4|A");
        buf.record_hit("v1|address|4|4|A");
        buf.record(Counter::Misses);
        assert_eq!(buf.pending(Counter::Hits), 2);
        assert_eq!(
            store.stat("hits").unwrap(),
            0,
            "reads never see unflushed counts"
        );

        store.flush_stats(&buf).await.unwrap();
        assert_eq!(buf.pending(Counter::Hits), 0);
        buf.record_hit("v1|address|4|4|A");
        buf.record(Counter::UpstreamErr);
        store.flush_stats(&buf).await.unwrap();
        store.flush_stats(&buf).await.unwrap(); // empty flush is a no-op

        assert_eq!(store.stat("hit:v1|address|4|4|A").unwrap(), 3);
        let totals = store.stat_totals().unwrap();
        assert_eq!(totals["hits"], 3);
        assert_eq!(totals["misses"], 1);
        assert_eq!(totals["upstream_err"], 1);
        assert_eq!(totals["stale_hits"], 0);
        assert_eq!(totals.len(), Counter::ALL.len());
    }

    #[test]
    fn restore_puts_deltas_back() {
        let buf = StatsBuffer::default();
        buf.record_hit("k");
        let d = buf.drain();
        assert_eq!(buf.pending(Counter::Hits), 0);
        buf.restore(d);
        assert_eq!(buf.pending(Counter::Hits), 1);
        assert_eq!(buf.drain().len(), 2);
    }
}
