//! Job result downloads (PLAN.md §6.3.9): rows joined with the *current* cache entries
//! (so a later refresh-negatives upgrade shows up), falling back to the row's recorded
//! status when the entry is gone or cannot render the row.

use serde_json::{Value, json};

use super::{JobRecord, JobRow, RowInput, RowState};
use crate::{
    cache::{
        envelope::Status,
        views::{BatchLookup, batch_answer, decode},
    },
    census::{
        batch_csv::{
            CoordRow, RowResult, output_writer, write_address_row, write_coord_row, write_label_row,
        },
        types::ReturnType,
    },
    state::AppState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultFormat {
    Csv,
    Json,
}

/// What one row of the download says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Answer {
    Result(RowResult),
    Pending,
    Failed,
    /// Answered, but the entry is gone and the row recorded no status.
    Unknown,
}

impl Answer {
    fn label(&self) -> &'static str {
        match self {
            Self::Result(RowResult::Match(_)) => "Match",
            Self::Result(RowResult::NoMatch) => "No_Match",
            Self::Result(RowResult::Tie) => "Tie",
            Self::Pending => "Pending",
            Self::Failed => "Failed",
            Self::Unknown => "Unknown",
        }
    }
}

const PAGE: u32 = 5000;

fn answer(st: &AppState, job: &JobRecord, row: &JobRow, bytes: Option<&[u8]>) -> Answer {
    match row.state {
        RowState::Pending => return Answer::Pending,
        RowState::Failed => return Answer::Failed,
        RowState::Cached | RowState::Done => {}
    }
    let current = bytes
        .and_then(|b| decode(b, st.codec).ok())
        .map(|s| batch_answer(&s.entry, job.batch_need(), &row.input.echo()));
    if let Some(BatchLookup::Hit { result, .. }) = current {
        return Answer::Result(result);
    }
    // The entry is gone or cannot render this row: say what the job recorded. A Match
    // then has no details (never invented).
    match row.status {
        Some(Status::Match) => Answer::Result(RowResult::Match(Box::default())),
        Some(Status::Tie) => Answer::Result(RowResult::Tie),
        Some(Status::NoMatch) => Answer::Result(RowResult::NoMatch),
        None => Answer::Unknown,
    }
}

fn input_json(input: &RowInput) -> Value {
    match input {
        RowInput::Coord { x, y } => json!({ "x": x, "y": y }),
        other => Value::String(other.echo()),
    }
}

fn write_csv(
    w: &mut csv::Writer<Vec<u8>>,
    job: &JobRecord,
    row: &JobRow,
    a: &Answer,
) -> csv::Result<()> {
    let geographies = job.return_type == ReturnType::Geographies;
    match (&row.input, a) {
        (RowInput::Coord { x, y }, Answer::Result(r)) => {
            let c = CoordRow {
                idx: 0,
                user_id: row.user_id.clone(),
                x_raw: x.clone(),
                y_raw: y.clone(),
                x: 0.0,
                y: 0.0,
            };
            write_coord_row(w, &c, r)
        }
        (RowInput::Coord { x, y }, other) => {
            write_label_row(w, &[&row.user_id, x, y], other.label())
        }
        (input, Answer::Result(r)) => {
            write_address_row(w, &row.user_id, &input.echo(), r, geographies)
        }
        (input, other) => write_label_row(w, &[&row.user_id, &input.echo()], other.label()),
    }
}

fn json_line(row: &JobRow, a: &Answer) -> Value {
    let mut o = json!({
        "id": row.user_id,
        "input": input_json(&row.input),
        "result": a.label(),
    });
    if let Answer::Result(RowResult::Match(v)) = a
        && let (Value::Object(m), Ok(Value::Object(view))) = (&mut o, serde_json::to_value(v))
    {
        m.extend(view.into_iter().filter(|(_, v)| !v.is_null()));
    }
    o
}

/// Render a job's rows. `only_unmatched` keeps rows that are not a Match (including
/// pending and failed ones), for resubmission.
pub fn render(
    st: &AppState,
    job: &JobRecord,
    format: ResultFormat,
    only_unmatched: bool,
) -> anyhow::Result<Vec<u8>> {
    let mut w = output_writer();
    let mut lines = Vec::new();
    let mut first = 0u32;
    loop {
        let rows = st
            .store
            .job_rows(&job.id, first, first.saturating_add(PAGE - 1))?;
        if rows.is_empty() {
            break;
        }
        let keys: Vec<&str> = rows.iter().map(|(_, r)| r.key.as_str()).collect();
        let entries = st.store.get_entries(&keys)?;
        for ((_, row), bytes) in rows.iter().zip(&entries) {
            let a = answer(st, job, row, bytes.as_deref());
            if only_unmatched && matches!(a, Answer::Result(RowResult::Match(_))) {
                continue;
            }
            match format {
                ResultFormat::Csv => write_csv(&mut w, job, row, &a)?,
                ResultFormat::Json => {
                    serde_json::to_writer(&mut lines, &json_line(row, &a))?;
                    lines.push(b'\n');
                }
            }
        }
        let Some(next) = rows.last().map(|(i, _)| i.saturating_add(1)) else {
            break;
        };
        if next == 0 || next == u32::MAX {
            break;
        }
        first = next;
    }
    Ok(match format {
        ResultFormat::Csv => w
            .into_inner()
            .map_err(|e| anyhow::anyhow!("csv writer: {e}"))?,
        ResultFormat::Json => lines,
    })
}
