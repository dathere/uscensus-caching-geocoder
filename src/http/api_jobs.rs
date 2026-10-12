//! `/api/v1/jobs` (PLAN.md §6.3, §8). Token-protected by the `/api/v1` router.

use axum::{
    Json,
    body::Bytes,
    extract::{FromRequest, Multipart, Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::batch::{
    ADDRESS_FIELDS, COORDINATE_FIELDS, ONELINE_FIELDS, Upload, bad_request, parse_rows,
    read_upload, resolve,
};
use crate::{
    batch::unix_now,
    census::{
        batch_csv::{address_rows, coord_rows, oneline_rows},
        types::ReturnType,
    },
    error::AppError,
    jobs::{
        Counts, JobKind, JobRecord, JobState, RowInput,
        refresh::{self, RefreshFilter},
        results::{self, ResultFormat},
        submit::{self, JobSpec, SubmitError},
    },
    state::SharedState,
};

pub const TAG: &str = "jobs";

pub fn router() -> OpenApiRouter<SharedState> {
    OpenApiRouter::new()
        .routes(routes!(submit_job, list_jobs))
        .routes(routes!(refresh_negatives))
        .routes(routes!(get_job, delete_job))
        .routes(routes!(pause_job))
        .routes(routes!(resume_job))
        .routes(routes!(cancel_job))
        .routes(routes!(job_results))
}

/// A job with derived progress.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct JobView {
    #[serde(flatten)]
    pub job: JobRecord,
    /// Percent of rows answered or failed.
    pub progress: f64,
    /// Estimated seconds left, from the upstream rate since the job started.
    pub eta_secs: Option<u64>,
}

impl From<JobRecord> for JobView {
    fn from(job: JobRecord) -> Self {
        let t = job.totals;
        let done = t.total.saturating_sub(t.pending);
        #[allow(clippy::cast_precision_loss)]
        let progress = if t.total == 0 {
            100.0
        } else {
            (done as f64 * 1000.0 / t.total as f64).round() / 10.0
        };
        let worked = done.saturating_sub(t.cached);
        let eta_secs = match (job.state, job.started_at) {
            (JobState::Running, Some(start)) if worked > 0 && t.pending > 0 => {
                let elapsed = unix_now().saturating_sub(start).max(1);
                Some(t.pending.saturating_mul(elapsed) / worked)
            }
            _ => None,
        };
        Self {
            job,
            progress,
            eta_secs,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SubmitResponse {
    pub id: String,
    pub state: JobState,
    pub counts: Counts,
}

/// JSON submission: one row shape per job.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SubmitJson {
    pub returntype: String,
    pub benchmark: String,
    #[serde(default)]
    pub vintage: Option<String>,
    #[serde(default)]
    pub priority: Option<i32>,
    pub rows: Vec<JsonRow>,
}

/// `{id, street, city?, state?, zip?}`, `{id, x, y}` or `{id, address}`; ids and
/// coordinates may be strings or numbers.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum JsonRow {
    Address {
        id: Value,
        street: String,
        #[serde(default)]
        city: String,
        #[serde(default)]
        state: String,
        #[serde(default)]
        zip: String,
    },
    Coord {
        id: Value,
        x: Value,
        y: Value,
    },
    Oneline {
        id: Value,
        address: String,
    },
}

/// A JSON scalar as text (numbers keep their exact digits).
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

impl JsonRow {
    fn into_input(self) -> (String, RowInput) {
        match self {
            Self::Address {
                id,
                street,
                city,
                state,
                zip,
            } => (
                text(&id),
                RowInput::Address {
                    street,
                    city,
                    state,
                    zip,
                },
            ),
            Self::Coord { id, x, y } => (
                text(&id),
                RowInput::Coord {
                    x: text(&x),
                    y: text(&y),
                },
            ),
            Self::Oneline { id, address } => (text(&id), RowInput::Oneline { address }),
        }
    }
}

/// Multipart submission (documentation only; parsed by hand).
#[derive(Debug, ToSchema)]
#[allow(dead_code, non_snake_case)]
pub struct JobUpload {
    /// `id, street, city, state, zip` (or `id, address` with `searchtype=onelineaddress`).
    #[schema(value_type = Option<String>, format = Binary)]
    addressFile: Option<Vec<u8>>,
    /// `id, x, y`.
    #[schema(value_type = Option<String>, format = Binary)]
    coordinatesFile: Option<Vec<u8>>,
    /// `locations` or `geographies`.
    returntype: String,
    /// `address` (default), `onelineaddress` or `coordinates`.
    searchtype: Option<String>,
    benchmark: String,
    vintage: Option<String>,
    priority: Option<i32>,
}

fn submit_error(e: SubmitError) -> Response {
    match e {
        SubmitError::Invalid(msg) => AppError::BadRequest(msg).into_response(),
        SubmitError::Busy(msg) => AppError::Conflict(msg).into_response(),
        SubmitError::Store(e) => AppError::Internal(e.into()).into_response(),
        SubmitError::Internal(e) => AppError::Internal(e).into_response(),
    }
}

fn parse_return_type(s: Option<&str>) -> Result<ReturnType, Box<Response>> {
    s.and_then(ReturnType::parse)
        .ok_or_else(|| bad_request("returntype must be locations or geographies"))
}

/// Submit a job (multipart upload or JSON rows).
#[utoipa::path(
    post,
    path = "/api/v1/jobs",
    tag = TAG,
    request_body(content(
        (SubmitJson = "application/json"),
        (JobUpload = "multipart/form-data")
    )),
    responses(
        (status = 201, description = "Job created", body = SubmitResponse),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Missing or wrong bearer token")
    )
)]
async fn submit_job(State(st): State<SharedState>, req: Request) -> Response {
    let multipart = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("multipart/form-data"));
    let prepared = if multipart {
        match Multipart::from_request(req, &st).await {
            Ok(mp) => from_multipart(&st, mp).await,
            Err(e) => return e.into_response(),
        }
    } else {
        match Json::<SubmitJson>::from_request(req, &st).await {
            Ok(Json(body)) => from_json(&st, body),
            Err(e) => return AppError::BadRequest(e.body_text()).into_response(),
        }
    };
    let (spec, inputs) = match prepared {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let created = match submit::build_rows(&spec, inputs) {
        Ok((search_type, rows)) => submit::create(&st, spec, search_type, rows, true).await,
        Err(e) => Err(e),
    };
    match created {
        Ok(job) => (
            StatusCode::CREATED,
            Json(SubmitResponse {
                id: job.id,
                state: job.state,
                counts: job.totals,
            }),
        )
            .into_response(),
        Err(e) => submit_error(e),
    }
}

type Prepared = Result<(JobSpec, Vec<(String, RowInput)>), Box<Response>>;

fn spec(
    st: &SharedState,
    rt: ReturnType,
    benchmark: Option<&str>,
    vintage: Option<&str>,
    priority: i32,
    original_filename: Option<String>,
) -> Result<JobSpec, Box<Response>> {
    let (bench, vintage) = resolve(st, benchmark, vintage, rt)?;
    Ok(JobSpec {
        kind: JobKind::Geocode,
        return_type: rt,
        bench,
        vintage,
        original_filename,
        priority,
    })
}

async fn from_multipart(st: &SharedState, mp: Multipart) -> Prepared {
    let up: Upload = read_upload(mp, &["addressFile", "coordinatesFile"]).await?;
    let rt = parse_return_type(up.field("returntype"))?;
    let file_field = up.file.as_ref().map(|(f, _, _)| f.as_str());
    let search = match up.field("searchtype") {
        Some(s) => s,
        None if file_field == Some("coordinatesFile") => "coordinates",
        None => "address",
    };
    let priority = match up.field("priority").map(str::parse::<i32>) {
        None => 0,
        Some(Ok(p)) => p,
        Some(Err(_)) => return Err(bad_request("priority must be an integer")),
    };
    let filename = up.file.as_ref().and_then(|(_, n, _)| n.clone());
    let spec = spec(
        st,
        rt,
        up.field("benchmark"),
        up.field("vintage"),
        priority,
        filename,
    )?;
    let inputs = inputs_from_upload(&up, search).await?;
    Ok((spec, inputs))
}

/// Parse an uploaded job file (`search`: `address`, `onelineaddress` or `coordinates`)
/// into job inputs.
pub(crate) async fn inputs_from_upload(
    up: &Upload,
    search: &str,
) -> Result<Vec<(String, RowInput)>, Box<Response>> {
    Ok(match search {
        "address" => parse_rows(up, address_rows, usize::MAX, ADDRESS_FIELDS)
            .await?
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
        "onelineaddress" => parse_rows(up, oneline_rows, usize::MAX, ONELINE_FIELDS)
            .await?
            .into_iter()
            .map(|r| (r.user_id, RowInput::Oneline { address: r.address }))
            .collect(),
        "coordinates" => parse_rows(up, coord_rows, usize::MAX, COORDINATE_FIELDS)
            .await?
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
        _ => {
            return Err(bad_request(
                "searchtype must be address, onelineaddress or coordinates",
            ));
        }
    })
}

fn from_json(st: &SharedState, body: SubmitJson) -> Prepared {
    let rt = parse_return_type(Some(&body.returntype))?;
    let spec = spec(
        st,
        rt,
        Some(&body.benchmark),
        body.vintage.as_deref(),
        body.priority.unwrap_or(0),
        None,
    )?;
    Ok((
        spec,
        body.rows.into_iter().map(JsonRow::into_input).collect(),
    ))
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    pub state: Option<JobState>,
    pub limit: Option<usize>,
    /// The last id of the previous page.
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct JobList {
    pub jobs: Vec<JobView>,
    pub next_cursor: Option<String>,
}

/// List jobs, oldest first.
#[utoipa::path(
    get,
    path = "/api/v1/jobs",
    tag = TAG,
    params(ListQuery),
    responses((status = 200, body = JobList))
)]
async fn list_jobs(State(st): State<SharedState>, Query(q): Query<ListQuery>) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    let all = match st.store.job_list() {
        Ok(j) => j,
        Err(e) => return AppError::Internal(e.into()).into_response(),
    };
    let mut page: Vec<JobView> = all
        .into_iter()
        .filter(|j| q.state.is_none_or(|s| j.state == s))
        .filter(|j| q.cursor.as_ref().is_none_or(|c| j.id.as_str() > c.as_str()))
        .take(limit + 1)
        .map(JobView::from)
        .collect();
    let next_cursor = if page.len() > limit {
        page.truncate(limit);
        page.last().map(|v| v.job.id.clone())
    } else {
        None
    };
    Json(JobList {
        jobs: page,
        next_cursor,
    })
    .into_response()
}

fn not_found() -> Response {
    AppError::NotFound("Job not found".into()).into_response()
}

/// One job with progress and ETA.
#[utoipa::path(
    get,
    path = "/api/v1/jobs/{id}",
    tag = TAG,
    params(("id" = String, Path, description = "Job id")),
    responses((status = 200, body = JobView), (status = 404, description = "No such job"))
)]
async fn get_job(State(st): State<SharedState>, Path(id): Path<String>) -> Response {
    match st.store.job_get(&id) {
        Ok(Some(j)) => Json(JobView::from(j)).into_response(),
        Ok(None) => not_found(),
        Err(e) => AppError::Internal(e.into()).into_response(),
    }
}

/// Delete a job with its rows (cached answers stay).
#[utoipa::path(
    delete,
    path = "/api/v1/jobs/{id}",
    tag = TAG,
    params(("id" = String, Path, description = "Job id")),
    responses((status = 204, description = "Deleted"), (status = 404, description = "No such job"))
)]
async fn delete_job(State(st): State<SharedState>, Path(id): Path<String>) -> Response {
    match st.store.job_delete(id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(e) => AppError::Internal(e.into()).into_response(),
    }
}

async fn transition(
    st: &SharedState,
    id: String,
    f: impl Fn(&mut JobRecord) -> Result<(), String> + Send + Sync + 'static,
) -> Response {
    match st.store.job_update(id, f).await {
        Ok(Some(Ok(j))) => {
            st.jobs_notify.notify_one();
            Json(JobView::from(j)).into_response()
        }
        Ok(Some(Err(msg))) => AppError::Conflict(msg).into_response(),
        Ok(None) => not_found(),
        Err(e) => AppError::Internal(e.into()).into_response(),
    }
}

fn state_name(s: JobState) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Pause a queued or running job (the chunk in flight finishes).
#[utoipa::path(
    post,
    path = "/api/v1/jobs/{id}/pause",
    tag = TAG,
    params(("id" = String, Path, description = "Job id")),
    responses(
        (status = 200, body = JobView),
        (status = 404, description = "No such job"),
        (status = 409, description = "Not allowed in the job's state")
    )
)]
async fn pause_job(State(st): State<SharedState>, Path(id): Path<String>) -> Response {
    transition(&st, id, |j| {
        if !j.state.is_runnable() {
            return Err(format!("cannot pause a {} job", state_name(j.state)));
        }
        j.state = JobState::Paused;
        j.updated_at = unix_now();
        Ok(())
    })
    .await
}

/// Resume a paused job.
#[utoipa::path(
    post,
    path = "/api/v1/jobs/{id}/resume",
    tag = TAG,
    params(("id" = String, Path, description = "Job id")),
    responses(
        (status = 200, body = JobView),
        (status = 404, description = "No such job"),
        (status = 409, description = "Not allowed in the job's state")
    )
)]
async fn resume_job(State(st): State<SharedState>, Path(id): Path<String>) -> Response {
    transition(&st, id, |j| {
        if j.state != JobState::Paused {
            return Err(format!("cannot resume a {} job", state_name(j.state)));
        }
        j.state = if j.started_at.is_some() {
            JobState::Running
        } else {
            JobState::Queued
        };
        j.updated_at = unix_now();
        Ok(())
    })
    .await
}

/// Cancel a job; pending rows stay pending.
#[utoipa::path(
    post,
    path = "/api/v1/jobs/{id}/cancel",
    tag = TAG,
    params(("id" = String, Path, description = "Job id")),
    responses(
        (status = 200, body = JobView),
        (status = 404, description = "No such job"),
        (status = 409, description = "Not allowed in the job's state")
    )
)]
async fn cancel_job(State(st): State<SharedState>, Path(id): Path<String>) -> Response {
    transition(&st, id, |j| {
        if !j.state.is_active() {
            return Err(format!("cannot cancel a {} job", state_name(j.state)));
        }
        let now = unix_now();
        j.state = JobState::Cancelled;
        j.finished_at = Some(now);
        j.updated_at = now;
        Ok(())
    })
    .await
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ResultsQuery {
    /// `csv` (default) or `json` (JSON lines).
    pub format: Option<String>,
    /// `unmatched`: only rows that are not a Match.
    pub only: Option<String>,
}

/// Rows joined with the current cache entries, as batch CSV or JSON lines.
#[utoipa::path(
    get,
    path = "/api/v1/jobs/{id}/results",
    tag = TAG,
    params(("id" = String, Path, description = "Job id"), ResultsQuery),
    responses(
        (status = 200, description = "Results", content(
            (String = "text/plain"),
            (String = "application/x-ndjson")
        )),
        (status = 404, description = "No such job")
    )
)]
async fn job_results(
    State(st): State<SharedState>,
    Path(id): Path<String>,
    Query(q): Query<ResultsQuery>,
) -> Response {
    let format = match q.format.as_deref() {
        None | Some("csv") => ResultFormat::Csv,
        Some("json") => ResultFormat::Json,
        Some(_) => {
            return AppError::BadRequest("format must be csv or json".into()).into_response();
        }
    };
    let only_unmatched = match q.only.as_deref() {
        None => false,
        Some("unmatched") => true,
        Some(_) => return AppError::BadRequest("only must be unmatched".into()).into_response(),
    };
    let job = match st.store.job_get(&id) {
        Ok(Some(j)) => j,
        Ok(None) => return not_found(),
        Err(e) => return AppError::Internal(e.into()).into_response(),
    };
    let st2 = st.clone();
    let rendered = tokio::task::spawn_blocking(move || {
        results::render(&st2, &job, format, only_unmatched).map(|b| (b, job.id))
    })
    .await;
    let (body, id) = match rendered {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => return AppError::Internal(e).into_response(),
        Err(e) => return AppError::Internal(anyhow::anyhow!("results task: {e}")).into_response(),
    };
    let (ct, ext) = match format {
        ResultFormat::Csv => ("text/plain", "csv"),
        ResultFormat::Json => ("application/x-ndjson", "jsonl"),
    };
    let mut resp = (StatusCode::OK, body).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=job-{id}.{ext}")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    resp
}

#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct RefreshRequest {
    pub benchmark: Option<String>,
    pub vintage: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RefreshResponse {
    pub jobs: Vec<JobView>,
}

/// Create refresh-negatives jobs for due NoMatch keys (409 while one is active).
#[utoipa::path(
    post,
    path = "/api/v1/jobs/refresh-negatives",
    tag = TAG,
    request_body(content = Option<RefreshRequest>, content_type = "application/json"),
    responses(
        (status = 201, body = RefreshResponse),
        (status = 400, description = "Invalid filter"),
        (status = 409, description = "A refresh job is already active")
    )
)]
async fn refresh_negatives(State(st): State<SharedState>, body: Bytes) -> Response {
    let req: RefreshRequest = if body.iter().all(u8::is_ascii_whitespace) {
        RefreshRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => return AppError::BadRequest(e.to_string()).into_response(),
        }
    };
    let catalog = st.catalog.load();
    let bench = match req.benchmark.as_deref() {
        None => None,
        Some(b) => match catalog.resolve_benchmark(b) {
            Some(b) => Some(b.id),
            None => {
                return AppError::BadRequest(super::batch::MSG_INVALID_BENCHMARK.into())
                    .into_response();
            }
        },
    };
    let vintage = match (req.vintage.as_deref(), bench) {
        (None, _) => None,
        (Some(_), None) => {
            return AppError::BadRequest("vintage requires benchmark".into()).into_response();
        }
        (Some(v), Some(b)) => match catalog.resolve_vintage(b, v) {
            Some(v) => Some(v.id),
            None => {
                return AppError::BadRequest(super::batch::MSG_INVALID_VINTAGE.into())
                    .into_response();
            }
        },
    };
    let filter = RefreshFilter {
        benchmark_id: bench,
        vintage_id: vintage,
        limit: req.limit,
    };
    match refresh::create_jobs(&st, filter).await {
        Ok(jobs) => (
            StatusCode::CREATED,
            Json(RefreshResponse {
                jobs: jobs.into_iter().map(JobView::from).collect(),
            }),
        )
            .into_response(),
        Err(e) => submit_error(e),
    }
}
