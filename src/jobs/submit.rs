//! Job submission (PLAN.md §6.3.1): key every row, mark rows the cache already
//! answers as `Cached`, lay out chunks, and persist durably.

use std::time::SystemTime;

use crate::{
    batch::{self, unix_now},
    cache::{
        key::{CacheKey, SearchType},
        views::{BatchLookup, Lookup, classify},
    },
    catalog::{Benchmark, Vintage},
    census::{batch_csv::RowResult, types::ReturnType},
    state::AppState,
    store::StoreError,
};

use super::{
    ChunkState, ChunkStatus, Counts, JobKind, JobRecord, JobRow, JobState, RowInput, RowState,
};

#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    /// The request is not acceptable (rendered as 400).
    #[error("{0}")]
    Invalid(String),
    /// Conflicts with work already in progress (rendered as 409).
    #[error("{0}")]
    Busy(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// What a new job geocodes.
pub struct JobSpec {
    pub kind: JobKind,
    pub return_type: ReturnType,
    pub bench: Benchmark,
    /// The vintage in the cache keys (the benchmark default for `locations`).
    pub vintage: Vintage,
    pub original_filename: Option<String>,
    pub priority: i32,
}

pub fn result_status(r: &RowResult) -> crate::cache::envelope::Status {
    use crate::cache::envelope::Status;
    match r {
        RowResult::Match(_) => Status::Match,
        RowResult::NoMatch => Status::NoMatch,
        RowResult::Tie => Status::Tie,
    }
}

/// Parse a job file (`search`: `address`, `onelineaddress` or `coordinates`) into job
/// inputs; `.xls`/`.xlsx` by file name with the `xlsx` feature. Blocking.
pub fn inputs_from_file(
    data: &[u8],
    filename: Option<&str>,
    search: &str,
) -> Result<Vec<(String, RowInput)>, String> {
    use crate::census::batch_csv::{
        MAX_INPUT_FIELDS, address_rows, coord_rows, oneline_rows, read_records,
    };
    let read =
        || read_records(data, filename, usize::MAX, MAX_INPUT_FIELDS).map_err(|e| e.to_string());
    let inputs: Vec<(String, RowInput)> = match search {
        "address" => address_rows(read()?)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|r| {
                (
                    r.user_id,
                    RowInput::Address {
                        street: r.street,
                        city: r.city,
                        state: r.state,
                        zip: r.zip,
                    },
                )
            })
            .collect(),
        "onelineaddress" => oneline_rows(read()?)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|r| (r.user_id, RowInput::Oneline { address: r.address }))
            .collect(),
        "coordinates" => coord_rows(read()?)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|r| {
                (
                    r.user_id,
                    RowInput::Coord {
                        x: r.x_raw,
                        y: r.y_raw,
                    },
                )
            })
            .collect(),
        _ => return Err("searchtype must be address, onelineaddress or coordinates".into()),
    };
    if inputs.is_empty() {
        return Err("Batch file contains no records".into());
    }
    Ok(inputs)
}

/// Key `(user id, input)` rows. All rows must share one search type (the job's), and
/// coordinates must be numbers and need `geographies`.
pub fn build_rows(
    spec: &JobSpec,
    inputs: Vec<(String, RowInput)>,
) -> Result<(SearchType, Vec<JobRow>), SubmitError> {
    let Some(search_type) = inputs.first().map(|(_, i)| i.search_type()) else {
        return Err(SubmitError::Invalid("Job contains no records".into()));
    };
    if search_type == SearchType::Coordinates && spec.return_type == ReturnType::Locations {
        return Err(SubmitError::Invalid(
            "Coordinates require returntype geographies".into(),
        ));
    }
    let mut bad = Vec::new();
    let mut rows = Vec::with_capacity(inputs.len());
    for (n, (user_id, input)) in (1usize..).zip(inputs) {
        let key = match input.key_input() {
            Some(k) if input.search_type() == search_type => k,
            _ => {
                bad.push(n);
                continue;
            }
        };
        rows.push(JobRow {
            user_id,
            key: CacheKey::new(spec.bench.id, spec.vintage.id, &key).to_string(),
            input,
            state: RowState::Pending,
            status: None,
            attempts: 0,
        });
    }
    if !bad.is_empty() {
        let shown: Vec<String> = bad.iter().take(50).map(ToString::to_string).collect();
        return Err(SubmitError::Invalid(format!(
            "Malformed rows (all rows must be {} records): {}",
            search_type.as_str(),
            shown.join(", ")
        )));
    }
    Ok((search_type, rows))
}

/// Mark rows the cache answers (fresh entries only) as `Cached`.
async fn precheck(
    st: &AppState,
    search_type: SearchType,
    rec: &JobRecord,
    rows: &mut [JobRow],
) -> Result<(), SubmitError> {
    let class = st.catalog.load().class(rec.benchmark_id);
    let keys: Vec<String> = rows.iter().map(|r| r.key.clone()).collect();
    if search_type == SearchType::Oneline {
        // Oneline rows are answered by single-record entries only.
        let (store, codec, cfg, need) = (
            st.store.clone(),
            st.codec,
            st.settings().config.clone(),
            rec.need(),
        );
        let statuses = tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            let now = SystemTime::now();
            Ok::<_, StoreError>(
                store
                    .get_entries(&refs)?
                    .iter()
                    .map(
                        |b| match classify(b.as_deref(), codec, &cfg, class, need, now) {
                            Lookup::Hit(s) => Some(s.entry.status),
                            _ => None,
                        },
                    )
                    .collect::<Vec<_>>(),
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!("job precheck task: {e}"))??;
        for (row, status) in rows.iter_mut().zip(statuses) {
            if status.is_some() {
                row.state = RowState::Cached;
                row.status = status;
            }
        }
    } else {
        let echoes = rows.iter().map(|r| r.input.echo()).collect();
        let lookups = batch::lookup(st, keys, echoes, rec.batch_need(), class).await?;
        for (row, l) in rows.iter_mut().zip(lookups) {
            if let BatchLookup::Hit { result, .. } = l {
                row.state = RowState::Cached;
                row.status = Some(result_status(&result));
            }
        }
    }
    Ok(())
}

/// Create a job from keyed rows. `precheck` answers rows from the cache first (geocode
/// jobs); refresh jobs skip it because their entries are deliberately re-fetched.
pub async fn create(
    st: &AppState,
    spec: JobSpec,
    search_type: SearchType,
    mut rows: Vec<JobRow>,
    precheck_cache: bool,
) -> Result<JobRecord, SubmitError> {
    if rows.is_empty() {
        return Err(SubmitError::Invalid("Job contains no records".into()));
    }
    let n = u32::try_from(rows.len())
        .map_err(|_| SubmitError::Invalid("Job contains too many records".into()))?;
    let chunk_size = u32::try_from(st.settings().config.jobs.chunk_size.max(1)).unwrap_or(9999);
    let now = unix_now();
    let mut rec = JobRecord {
        id: uuid::Uuid::now_v7().to_string(),
        kind: spec.kind,
        state: JobState::Queued,
        return_type: spec.return_type,
        search_type,
        benchmark_id: spec.bench.id,
        vintage_id: spec.vintage.id,
        created_at: now,
        updated_at: now,
        started_at: None,
        finished_at: None,
        totals: Counts::default(),
        chunk_size,
        chunks_total: n.div_ceil(chunk_size),
        original_filename: spec.original_filename,
        error: None,
        priority: spec.priority,
    };
    if precheck_cache {
        precheck(st, search_type, &rec, &mut rows).await?;
    }
    rec.totals = Counts::of(&rows);

    let chunks: Vec<ChunkState> = (0..rec.chunks_total)
        .map(|c| {
            let first = c * chunk_size;
            let last = (first + chunk_size).min(n) - 1;
            let pending = rows[first as usize..=last as usize]
                .iter()
                .any(|r| r.state == RowState::Pending);
            ChunkState {
                first_row: first,
                last_row: last,
                state: if pending {
                    ChunkStatus::Pending
                } else {
                    ChunkStatus::Done
                },
                lease_until: None,
                attempts: 0,
                last_error: None,
            }
        })
        .collect();
    if rec.totals.pending == 0 {
        rec.state = JobState::Completed;
        rec.finished_at = Some(now);
    }
    st.store.job_create(rec.clone(), rows, chunks).await?;
    st.jobs_notify.notify_one();
    Ok(rec)
}
