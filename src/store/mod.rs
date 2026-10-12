//! The redb store: one `Database`, inline reads, and a single writer thread.
//!
//! Reads run inline on the caller (a read txn + decode is ~100 µs; `spawn_blocking`
//! would cost more). The `Database` sits behind an `RwLock` only so [`Store::compact`]
//! can get the `&mut` redb requires: every read/write txn holds the read guard for its
//! whole lifetime, so compaction waits for in-flight txns and stalls new ones while it
//! runs (an admin-only operation).

pub mod catalog;
pub mod config_overrides;
pub mod entries;
pub mod jobs;
pub mod retry_queue;
pub mod stats;
pub mod tables;
pub mod writer;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, PoisonError, RwLock},
    time::Duration,
};

use redb::{Database, DatabaseError, ReadTransaction, ReadableDatabase, ReadableTable};
use tracing::warn;

use crate::{cache::codec::Codec, config::StoreConfig};
use writer::WriterHandle;
pub use writer::{Op, WriteCtx, WriterConfig};

/// redb file name suffix shared with `cached::RedbCache` (`DISK_FILE_VERSION = 3`).
pub const FILE_VERSION: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database {} is locked by another process (is the server running?)", path.display())]
    Locked { path: PathBuf },
    #[error("database {} has schema version {found}, this build expects {expected}", path.display())]
    Schema {
        path: PathBuf,
        found: String,
        expected: &'static str,
    },
    #[error("store writer has shut down")]
    Closed,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("redb: {0}")]
    Redb(#[from] redb::Error),
    #[error("encoding: {0}")]
    Encoding(String),
    #[error("write failed: {0}")]
    Write(String),
}

macro_rules! redb_errors {
    ($($t:ty),*) => {$(
        impl From<$t> for StoreError {
            fn from(e: $t) -> Self {
                Self::Redb(e.into())
            }
        }
    )*};
}
redb_errors!(
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    redb::CompactionError,
    redb::SetDurabilityError,
    DatabaseError
);

#[derive(Debug, Clone)]
pub struct StoreOptions {
    pub dir: PathBuf,
    pub name: String,
    pub cache_mb: u64,
    pub writer: WriterConfig,
}

impl From<&StoreConfig> for StoreOptions {
    fn from(c: &StoreConfig) -> Self {
        Self {
            dir: c.db_dir.clone(),
            name: c.cache_name.clone(),
            cache_mb: c.db_cache_mb,
            writer: WriterConfig {
                flush: Duration::from_millis(c.writer_flush_ms),
                max_batch: c.writer_max_batch,
                durable_every: Duration::from_secs(c.durable_commit_secs),
            },
        }
    }
}

impl StoreOptions {
    /// `<dir>/<name>_v3.redb`, the path `cached::RedbCache` uses for the same name.
    pub fn path(&self) -> PathBuf {
        self.dir.join(format!("{}_v{FILE_VERSION}.redb", self.name))
    }

    pub fn with_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir = dir.into();
        self
    }
}

/// Cheap to clone; the writer is shut down (durably) when the last clone drops.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

// Field order matters: `writer` drops (drain + durable commit + join) before `db`.
struct Inner {
    writer: WriterHandle,
    db: Arc<RwLock<Database>>,
    path: PathBuf,
}

impl Store {
    pub fn open(opts: &StoreOptions) -> Result<Self, StoreError> {
        std::fs::create_dir_all(&opts.dir)?;
        if std::fs::symlink_metadata(&opts.dir)?
            .file_type()
            .is_symlink()
        {
            warn!(
                dir = %opts.dir.display(),
                "db_dir is a symlink: the server works, but cached/qsv tooling refuses symlinked cache dirs"
            );
        }
        let path = opts.path();
        let cache_bytes =
            usize::try_from(opts.cache_mb.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX);
        let db = Database::builder()
            .set_cache_size(cache_bytes)
            .create(&path)
            .map_err(|e| match e {
                DatabaseError::DatabaseAlreadyOpen => StoreError::Locked { path: path.clone() },
                other => other.into(),
            })?;
        init_tables(&db, &path)?;
        let db = Arc::new(RwLock::new(db));
        let writer = WriterHandle::spawn(Arc::clone(&db), opts.writer)?;
        Ok(Self {
            inner: Arc::new(Inner { writer, db, path }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    pub fn file_size(&self) -> Result<u64, StoreError> {
        Ok(std::fs::metadata(&self.inner.path)?.len())
    }

    /// Run `f` in a read transaction; the lock guard outlives the transaction.
    pub fn read<T>(
        &self,
        f: impl FnOnce(&ReadTransaction) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let guard = self.inner.db.read().unwrap_or_else(PoisonError::into_inner);
        let txn = guard.begin_read()?;
        let out = f(&txn);
        drop(txn);
        drop(guard);
        out
    }

    /// Queue a mutation and wait for the commit that contains it.
    pub async fn write(&self, op: Op) -> Result<(), StoreError> {
        self.inner.writer.write(op).await
    }

    /// Make every write queued so far durable.
    pub async fn flush(&self) -> Result<(), StoreError> {
        self.inner.writer.flush().await
    }

    /// `true` if a read transaction can be opened (for `/healthz`).
    pub fn is_healthy(&self) -> bool {
        self.read(|_| Ok(())).is_ok()
    }

    /// Reclaim free space. Blocks reads and writes while it runs; call off the async
    /// runtime (`spawn_blocking`). Returns `false` when there was nothing to compact.
    pub fn compact(&self) -> Result<bool, StoreError> {
        let mut guard = self
            .inner
            .db
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        Ok(guard.compact()?)
    }
}

fn init_tables(db: &Database, path: &Path) -> Result<(), StoreError> {
    let txn = db.begin_write()?;
    {
        txn.open_table(tables::ENTRIES)?;
        txn.open_table(tables::JOBS)?;
        txn.open_table(tables::JOB_ROWS)?;
        txn.open_table(tables::JOB_CHUNKS)?;
        txn.open_table(tables::STATS)?;
        txn.open_table(tables::CATALOG)?;
        txn.open_table(tables::RETRY_QUEUE)?;
        txn.open_table(tables::CONFIG_OVERRIDES)?;
        let mut meta = txn.open_table(tables::META)?;
        let found = meta.get("schema_version")?.map(|g| g.value().to_owned());
        match found {
            Some(v) if v != tables::SCHEMA_VERSION => {
                return Err(StoreError::Schema {
                    path: path.to_owned(),
                    found: v,
                    expected: tables::SCHEMA_VERSION,
                });
            }
            Some(_) => {}
            None => {
                meta.insert("schema_version", tables::SCHEMA_VERSION)?;
            }
        }
        let dict_id = Codec::embedded().current_dict_id().to_string();
        meta.insert("dict_id", dict_id.as_str())?;
    }
    txn.commit()?;
    Ok(())
}

impl Store {
    /// A `META` value (`schema_version`, `dict_id`).
    pub fn meta(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.read(|txn| {
            let t = txn.open_table(tables::META)?;
            Ok(t.get(key)?.map(|g| g.value().to_owned()))
        })
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;

    pub(crate) fn opts(dir: &Path) -> StoreOptions {
        StoreOptions {
            dir: dir.to_owned(),
            name: "geocode-census".into(),
            cache_mb: 8,
            writer: WriterConfig {
                flush: Duration::from_millis(5),
                max_batch: 64,
                durable_every: Duration::from_secs(30),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{test_util::opts, *};

    #[tokio::test]
    async fn open_creates_file_and_meta() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&opts(dir.path())).unwrap();
        assert_eq!(store.path(), dir.path().join("geocode-census_v3.redb"));
        assert_eq!(store.meta("schema_version").unwrap().as_deref(), Some("1"));
        assert_eq!(
            store.meta("dict_id").unwrap(),
            Some(Codec::embedded().current_dict_id().to_string())
        );
        assert!(store.is_healthy());
    }

    #[tokio::test]
    async fn second_open_reports_lock() {
        let dir = tempfile::tempdir().unwrap();
        let _a = Store::open(&opts(dir.path())).unwrap();
        let err = Store::open(&opts(dir.path())).err().unwrap();
        assert!(matches!(err, StoreError::Locked { .. }), "{err}");
        assert!(err.to_string().contains("is the server running"));
    }

    #[tokio::test]
    async fn schema_mismatch_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(&opts(dir.path())).unwrap();
            store
                .write(Box::new(|ctx| {
                    ctx.meta()?.insert("schema_version", "99")?;
                    Ok(())
                }))
                .await
                .unwrap();
        }
        let err = Store::open(&opts(dir.path())).err().unwrap();
        assert!(
            matches!(err, StoreError::Schema { ref found, .. } if found == "99"),
            "{err}"
        );
    }
}
