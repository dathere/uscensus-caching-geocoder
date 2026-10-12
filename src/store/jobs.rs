//! Job tables: `JOBS` (record), `JOB_ROWS` and `JOB_CHUNKS` keyed by `(job id, idx)`.

use std::sync::{Arc, Mutex, PoisonError};

use redb::ReadableTable;
use serde::{Serialize, de::DeserializeOwned};

use super::{
    Store, StoreError,
    tables::{JOB_CHUNKS, JOB_ROWS, JOBS},
    writer::WriteCtx,
};
use crate::jobs::{ChunkState, JobRecord, JobRow};

pub(crate) fn enc<T: Serialize>(v: &T) -> Result<Vec<u8>, StoreError> {
    rmp_serde::to_vec_named(v).map_err(|e| StoreError::Encoding(e.to_string()))
}

pub(crate) fn dec<T: DeserializeOwned>(b: &[u8]) -> Result<T, StoreError> {
    rmp_serde::from_slice(b).map_err(|e| StoreError::Encoding(e.to_string()))
}

impl WriteCtx<'_> {
    pub fn job_record(&mut self, id: &str) -> Result<Option<JobRecord>, StoreError> {
        match self.jobs()?.get(id)? {
            Some(v) => Ok(Some(dec(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn put_job_record(&mut self, rec: &JobRecord) -> Result<(), StoreError> {
        let bytes = enc(rec)?;
        self.jobs()?.insert(rec.id.as_str(), bytes.as_slice())?;
        Ok(())
    }

    pub fn job_row(&mut self, id: &str, idx: u32) -> Result<Option<JobRow>, StoreError> {
        match self.job_rows()?.get((id, idx))? {
            Some(v) => Ok(Some(dec(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn put_job_row(&mut self, id: &str, idx: u32, row: &JobRow) -> Result<(), StoreError> {
        let bytes = enc(row)?;
        self.job_rows()?.insert((id, idx), bytes.as_slice())?;
        Ok(())
    }

    pub fn job_chunk(&mut self, id: &str, idx: u32) -> Result<Option<ChunkState>, StoreError> {
        match self.job_chunks()?.get((id, idx))? {
            Some(v) => Ok(Some(dec(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn put_job_chunk(
        &mut self,
        id: &str,
        idx: u32,
        chunk: &ChunkState,
    ) -> Result<(), StoreError> {
        let bytes = enc(chunk)?;
        self.job_chunks()?.insert((id, idx), bytes.as_slice())?;
        Ok(())
    }
}

impl Store {
    pub fn job_get(&self, id: &str) -> Result<Option<JobRecord>, StoreError> {
        self.read(|txn| {
            let t = txn.open_table(JOBS)?;
            match t.get(id)? {
                Some(v) => Ok(Some(dec(v.value())?)),
                None => Ok(None),
            }
        })
    }

    /// Every job, oldest first (UUIDv7 ids sort by creation time).
    pub fn job_list(&self) -> Result<Vec<JobRecord>, StoreError> {
        self.read(|txn| {
            let t = txn.open_table(JOBS)?;
            let mut out = Vec::new();
            for item in t.iter()? {
                let (_, v) = item?;
                out.push(dec(v.value())?);
            }
            Ok(out)
        })
    }

    /// Rows `first..=last` of a job, in index order.
    pub fn job_rows(
        &self,
        id: &str,
        first: u32,
        last: u32,
    ) -> Result<Vec<(u32, JobRow)>, StoreError> {
        self.read(|txn| {
            let t = txn.open_table(JOB_ROWS)?;
            let mut out = Vec::new();
            for item in t.range((id, first)..=(id, last))? {
                let (k, v) = item?;
                out.push((k.value().1, dec(v.value())?));
            }
            Ok(out)
        })
    }

    pub fn job_chunks(&self, id: &str) -> Result<Vec<(u32, ChunkState)>, StoreError> {
        self.read(|txn| {
            let t = txn.open_table(JOB_CHUNKS)?;
            let mut out = Vec::new();
            for item in t.range((id, 0)..=(id, u32::MAX))? {
                let (k, v) = item?;
                out.push((k.value().1, dec(v.value())?));
            }
            Ok(out)
        })
    }

    /// Persist a new job with its rows and chunks, durably.
    pub async fn job_create(
        &self,
        rec: JobRecord,
        rows: Vec<JobRow>,
        chunks: Vec<ChunkState>,
    ) -> Result<(), StoreError> {
        let id = rec.id.clone();
        let rec = enc(&rec)?;
        let rows = rows.iter().map(enc).collect::<Result<Vec<_>, _>>()?;
        let chunks = chunks.iter().map(enc).collect::<Result<Vec<_>, _>>()?;
        self.write(Box::new(move |ctx| {
            ctx.jobs()?.insert(id.as_str(), rec.as_slice())?;
            let t = ctx.job_rows()?;
            for (i, r) in (0u32..).zip(&rows) {
                t.insert((id.as_str(), i), r.as_slice())?;
            }
            let t = ctx.job_chunks()?;
            for (i, c) in (0u32..).zip(&chunks) {
                t.insert((id.as_str(), i), c.as_slice())?;
            }
            Ok(())
        }))
        .await?;
        self.flush().await
    }

    /// Remove a job with its rows and chunks; `false` if it did not exist.
    pub async fn job_delete(&self, id: String) -> Result<bool, StoreError> {
        let existed = Arc::new(Mutex::new(false));
        let flag = Arc::clone(&existed);
        self.write(Box::new(move |ctx| {
            let found = ctx.jobs()?.remove(id.as_str())?.is_some();
            *flag.lock().unwrap_or_else(PoisonError::into_inner) = found;
            let range = (id.as_str(), 0)..=(id.as_str(), u32::MAX);
            ctx.job_rows()?.retain_in(range.clone(), |_, _| false)?;
            ctx.job_chunks()?.retain_in(range, |_, _| false)?;
            Ok(())
        }))
        .await?;
        Ok(*existed.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Apply `f` to a job record inside a write transaction. `f` returns `Err(msg)` to
    /// refuse (nothing is written). `Ok(None)` if the job does not exist.
    pub async fn job_update(
        &self,
        id: String,
        f: impl Fn(&mut JobRecord) -> Result<(), String> + Send + Sync + 'static,
    ) -> Result<Option<Result<JobRecord, String>>, StoreError> {
        let out = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&out);
        self.write(Box::new(move |ctx| {
            let result = match ctx.job_record(&id)? {
                None => None,
                Some(mut rec) => match f(&mut rec) {
                    Ok(()) => {
                        ctx.put_job_record(&rec)?;
                        Some(Ok(rec))
                    }
                    Err(e) => Some(Err(e)),
                },
            };
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = result;
            Ok(())
        }))
        .await?;
        Ok(out.lock().unwrap_or_else(PoisonError::into_inner).take())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::opts;
    use super::*;
    use crate::{
        cache::key::SearchType,
        census::types::ReturnType,
        jobs::{ChunkStatus, Counts, JobKind, JobState, RowInput, RowState},
    };

    fn record(id: &str) -> JobRecord {
        JobRecord {
            id: id.into(),
            kind: JobKind::Geocode,
            state: JobState::Queued,
            return_type: ReturnType::Locations,
            search_type: SearchType::Address,
            benchmark_id: 4,
            vintage_id: 4,
            created_at: 1,
            updated_at: 1,
            started_at: None,
            finished_at: None,
            totals: Counts::default(),
            chunk_size: 2,
            chunks_total: 1,
            original_filename: None,
            error: None,
            priority: 0,
        }
    }

    fn row(n: u32) -> JobRow {
        JobRow {
            user_id: n.to_string(),
            input: RowInput::Oneline {
                address: format!("{n} Main St"),
            },
            key: format!("k{n}"),
            state: RowState::Pending,
            status: None,
            attempts: 0,
        }
    }

    fn chunk() -> ChunkState {
        ChunkState {
            first_row: 0,
            last_row: 2,
            state: ChunkStatus::Pending,
            lease_until: None,
            attempts: 0,
            last_error: None,
        }
    }

    #[tokio::test]
    async fn create_read_update_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&opts(dir.path())).unwrap();
        store
            .job_create(record("a"), vec![row(0), row(1), row(2)], vec![chunk()])
            .await
            .unwrap();
        store
            .job_create(record("b"), vec![row(9)], vec![chunk()])
            .await
            .unwrap();

        assert_eq!(store.job_get("a").unwrap(), Some(record("a")));
        assert_eq!(
            store
                .job_list()
                .unwrap()
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        let rows = store.job_rows("a", 1, 2).unwrap();
        assert_eq!(rows, vec![(1, row(1)), (2, row(2))]);
        assert_eq!(store.job_chunks("b").unwrap(), vec![(0, chunk())]);

        let refused = store
            .job_update("a".into(), |_| Err("nope".into()))
            .await
            .unwrap();
        assert_eq!(refused, Some(Err("nope".into())));
        let updated = store
            .job_update("a".into(), |r| {
                r.state = JobState::Paused;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(updated.unwrap().unwrap().state, JobState::Paused);
        assert_eq!(store.job_get("a").unwrap().unwrap().state, JobState::Paused);
        assert_eq!(
            store.job_update("zz".into(), |_| Ok(())).await.unwrap(),
            None
        );

        assert!(store.job_delete("a".into()).await.unwrap());
        assert!(!store.job_delete("a".into()).await.unwrap());
        assert_eq!(store.job_get("a").unwrap(), None);
        assert!(store.job_rows("a", 0, u32::MAX).unwrap().is_empty());
        assert!(store.job_chunks("a").unwrap().is_empty());
        assert_eq!(store.job_rows("b", 0, u32::MAX).unwrap().len(), 1);
    }
}
