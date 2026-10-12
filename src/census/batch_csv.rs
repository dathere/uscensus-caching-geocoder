//! Batch CSV formats (PLAN.md §0.2, §6.2).
//!
//! Input: `id, street, city, state, zip` (addresses) or `id, x, y` (coordinates), no
//! header. Output (as captured live): no header, every field quoted, `\n` terminator.
//!
//! * address Match: `"id","input","Match","Exact|Non_Exact","matched","lon,lat","tigerLineId","side"`
//!   plus `"state","county","tract","block"` for `geographies`
//! * address No_Match: `"id","input","No_Match"`; Tie: `"id","input","Tie"` (Tie shape unverified)
//! * coordinates Match: `"id","x","y","Match","state","county","tract","block"`;
//!   No_Match: `"id","x","y","No_Match"` (unverified)

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value, value::RawValue};

use crate::cache::{
    envelope::{BatchView, Status},
    key::canonicalize_component,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BatchInputError {
    #[error("Malformed rows: {}", list(.0))]
    Malformed(Vec<usize>),
    #[error("Could not read batch file: {0}")]
    Unreadable(String),
}

fn list(rows: &[usize]) -> String {
    let shown: Vec<String> = rows.iter().take(50).map(ToString::to_string).collect();
    let more = rows.len().saturating_sub(50);
    if more > 0 {
        format!("{} (and {more} more)", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressRow {
    /// 1-based row number; the id sent upstream.
    pub idx: u32,
    pub user_id: String,
    pub street: String,
    pub city: String,
    pub state: String,
    pub zip: String,
}

impl AddressRow {
    /// The `input` column Census echoes: `street, city, state, zip` (empties kept).
    pub fn echo(&self) -> String {
        format!(
            "{}, {}, {}, {}",
            self.street, self.city, self.state, self.zip
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CoordRow {
    pub idx: u32,
    pub user_id: String,
    pub x_raw: String,
    pub y_raw: String,
    pub x: f64,
    pub y: f64,
}

/// One input record with its 1-based record number (blank lines are not counted).
pub type Record = (usize, Vec<String>);

/// Rows of the uploaded file as strings. `.xls`/`.xlsx` need the `xlsx` feature.
pub fn read_records(bytes: &[u8], filename: Option<&str>) -> Result<Vec<Record>, BatchInputError> {
    let is_excel = filename.is_some_and(|f| {
        let f = f.to_ascii_lowercase();
        f.ends_with(".xlsx") || f.ends_with(".xls") || f.ends_with(".xlsm") || f.ends_with(".xlsb")
    });
    if is_excel {
        return read_excel(bytes);
    }
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(bytes);
    let mut out = Vec::new();
    let mut bad = Vec::new();
    for (i, rec) in rdr.records().enumerate() {
        match rec {
            Ok(r) => out.push((i + 1, r.iter().map(str::to_owned).collect())),
            Err(_) => bad.push(i + 1),
        }
    }
    if bad.is_empty() {
        Ok(out)
    } else {
        Err(BatchInputError::Malformed(bad))
    }
}

#[cfg(feature = "xlsx")]
fn read_excel(bytes: &[u8]) -> Result<Vec<Record>, BatchInputError> {
    use calamine::{Data, Reader, open_workbook_auto_from_rs};
    let mut wb = open_workbook_auto_from_rs(std::io::Cursor::new(bytes.to_vec()))
        .map_err(|e| BatchInputError::Unreadable(e.to_string()))?;
    let range = wb
        .worksheet_range_at(0)
        .ok_or_else(|| BatchInputError::Unreadable("workbook has no sheets".into()))?
        .map_err(|e| BatchInputError::Unreadable(e.to_string()))?;
    Ok(range
        .rows()
        .enumerate()
        .map(|(i, row)| {
            let cells = row
                .iter()
                .map(|c| match c {
                    Data::Empty => String::new(),
                    Data::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{f:.0}"),
                    other => other.to_string(),
                })
                .collect();
            (i + 1, cells)
        })
        .collect())
}

#[cfg(not(feature = "xlsx"))]
fn read_excel(_bytes: &[u8]) -> Result<Vec<Record>, BatchInputError> {
    Err(BatchInputError::Unreadable(
        "Excel files are not supported by this build (enable the `xlsx` feature)".into(),
    ))
}

fn is_blank(r: &[String]) -> bool {
    r.iter().all(|f| f.trim().is_empty())
}

/// `id, street, city, state, zip`; missing trailing fields are empty. Rows with more
/// than five fields are malformed; blank rows are skipped.
pub fn address_rows(records: Vec<Record>) -> Result<Vec<AddressRow>, BatchInputError> {
    let mut rows = Vec::with_capacity(records.len());
    let mut bad = Vec::new();
    for (line, mut r) in records {
        if is_blank(&r) {
            continue;
        }
        if r.len() > 5 {
            bad.push(line);
            continue;
        }
        r.resize(5, String::new());
        let mut it = r.into_iter();
        let mut next = || it.next().unwrap_or_default();
        rows.push(AddressRow {
            idx: u32::try_from(line).unwrap_or(u32::MAX),
            user_id: next(),
            street: next(),
            city: next(),
            state: next(),
            zip: next(),
        });
    }
    if bad.is_empty() {
        Ok(rows)
    } else {
        Err(BatchInputError::Malformed(bad))
    }
}

/// `id, x, y` with finite numeric coordinates.
pub fn coord_rows(records: Vec<Record>) -> Result<Vec<CoordRow>, BatchInputError> {
    let mut rows = Vec::with_capacity(records.len());
    let mut bad = Vec::new();
    for (line, r) in records {
        if is_blank(&r) {
            continue;
        }
        let parsed = (r.len() == 3)
            .then(|| {
                (
                    r[1].trim().parse::<f64>().ok(),
                    r[2].trim().parse::<f64>().ok(),
                )
            })
            .and_then(|(x, y)| Some((x?, y?)))
            .filter(|(x, y)| x.is_finite() && y.is_finite());
        match parsed {
            Some((x, y)) => rows.push(CoordRow {
                idx: u32::try_from(line).unwrap_or(u32::MAX),
                user_id: r[0].clone(),
                x_raw: r[1].clone(),
                y_raw: r[2].clone(),
                x,
                y,
            }),
            None => bad.push(line),
        }
    }
    if bad.is_empty() {
        Ok(rows)
    } else {
        Err(BatchInputError::Malformed(bad))
    }
}

/// Body uploaded to Census: our row number as the id.
pub fn upload_addresses<'a>(rows: impl IntoIterator<Item = &'a AddressRow>) -> Vec<u8> {
    let mut w = csv::WriterBuilder::new().from_writer(Vec::new());
    for r in rows {
        let _ = w.write_record([&r.idx.to_string(), &r.street, &r.city, &r.state, &r.zip]);
    }
    w.into_inner().unwrap_or_default()
}

pub fn upload_coordinates<'a>(rows: impl IntoIterator<Item = &'a CoordRow>) -> Vec<u8> {
    let mut w = csv::WriterBuilder::new().from_writer(Vec::new());
    for r in rows {
        let _ = w.write_record([&r.idx.to_string(), r.x_raw.trim(), r.y_raw.trim()]);
    }
    w.into_inner().unwrap_or_default()
}

/// One parsed upstream result row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamRow {
    pub idx: u32,
    pub status: Status,
    pub view: BatchView,
}

fn status_of(s: &str) -> Option<Status> {
    match s.trim() {
        "Match" => Some(Status::Match),
        "No_Match" => Some(Status::NoMatch),
        "Tie" => Some(Status::Tie),
        _ => None,
    }
}

fn upstream_records(body: &[u8]) -> impl Iterator<Item = csv::StringRecord> + '_ {
    csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(body)
        .into_records()
        .filter_map(Result::ok)
}

fn field(r: &csv::StringRecord, i: usize) -> Option<String> {
    r.get(i).map(str::to_owned)
}

/// Parse an `addressbatch` result. Rows with ids we did not send are ignored.
pub fn parse_upstream_addresses(body: &[u8]) -> Vec<UpstreamRow> {
    upstream_records(body)
        .filter_map(|r| {
            let idx = r.get(0)?.trim().parse().ok()?;
            let status = status_of(r.get(2)?)?;
            let (lon, lat) = match r.get(5).and_then(|s| s.split_once(',')) {
                Some((lon, lat)) => (Some(lon.to_owned()), Some(lat.to_owned())),
                None => (None, None),
            };
            let view = if status == Status::Match {
                BatchView {
                    match_type: field(&r, 3),
                    matched_address: field(&r, 4),
                    lon,
                    lat,
                    tiger_line_id: field(&r, 6),
                    side: field(&r, 7),
                    state: field(&r, 8),
                    county: field(&r, 9),
                    tract: field(&r, 10),
                    block: field(&r, 11),
                }
            } else {
                BatchView::default()
            };
            Some(UpstreamRow { idx, status, view })
        })
        .collect()
}

/// Parse a `coordinatesbatch` result.
pub fn parse_upstream_coordinates(body: &[u8]) -> Vec<UpstreamRow> {
    upstream_records(body)
        .filter_map(|r| {
            let idx = r.get(0)?.trim().parse().ok()?;
            let status = status_of(r.get(3)?)?;
            let view = if status == Status::Match {
                BatchView {
                    state: field(&r, 4),
                    county: field(&r, 5),
                    tract: field(&r, 6),
                    block: field(&r, 7),
                    ..BatchView::default()
                }
            } else {
                BatchView::default()
            };
            Some(UpstreamRow { idx, status, view })
        })
        .collect()
}

/// What one output row says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowResult {
    Match(Box<BatchView>),
    NoMatch,
    Tie,
}

/// The output writer: every field quoted, `\n` line endings (as upstream).
pub fn output_writer() -> csv::Writer<Vec<u8>> {
    csv::WriterBuilder::new()
        .flexible(true)
        .quote_style(csv::QuoteStyle::Always)
        .terminator(csv::Terminator::Any(b'\n'))
        .from_writer(Vec::new())
}

fn s(o: &Option<String>) -> &str {
    o.as_deref().unwrap_or("")
}

pub fn write_address_row(
    w: &mut csv::Writer<Vec<u8>>,
    row: &AddressRow,
    result: &RowResult,
    geographies: bool,
) -> csv::Result<()> {
    let echo = row.echo();
    match result {
        RowResult::NoMatch => w.write_record([row.user_id.as_str(), &echo, "No_Match"]),
        RowResult::Tie => w.write_record([row.user_id.as_str(), &echo, "Tie"]),
        RowResult::Match(v) => {
            let lonlat = format!("{},{}", s(&v.lon), s(&v.lat));
            let mut rec = vec![
                row.user_id.as_str(),
                &echo,
                "Match",
                s(&v.match_type),
                s(&v.matched_address),
                &lonlat,
                s(&v.tiger_line_id),
                s(&v.side),
            ];
            if geographies {
                rec.extend([s(&v.state), s(&v.county), s(&v.tract), s(&v.block)]);
            }
            w.write_record(rec)
        }
    }
}

pub fn write_coord_row(
    w: &mut csv::Writer<Vec<u8>>,
    row: &CoordRow,
    result: &RowResult,
) -> csv::Result<()> {
    let head = [row.user_id.as_str(), row.x_raw.as_str(), row.y_raw.as_str()];
    match result {
        RowResult::Match(v) => w.write_record(head.into_iter().chain([
            "Match",
            s(&v.state),
            s(&v.county),
            s(&v.tract),
            s(&v.block),
        ])),
        RowResult::NoMatch => w.write_record(head.into_iter().chain(["No_Match"])),
        RowResult::Tie => w.write_record(head.into_iter().chain(["Tie"])),
    }
}

// ---------------------------------------------------------------- heuristics

#[derive(Deserialize)]
struct MatchJson<'a> {
    #[serde(rename = "matchedAddress")]
    matched_address: String,
    #[serde(borrow)]
    coordinates: CoordsJson<'a>,
    #[serde(rename = "tigerLine")]
    tiger_line: TigerLine,
    #[serde(borrow, default)]
    geographies: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct CoordsJson<'a> {
    #[serde(borrow)]
    x: &'a RawValue,
    #[serde(borrow)]
    y: &'a RawValue,
}

#[derive(Deserialize)]
struct TigerLine {
    #[serde(rename = "tigerLineId")]
    tiger_line_id: String,
    side: String,
}

/// STATE/COUNTY/TRACT/BLOCK of the first block in a `geographies` object
/// (`2020 Census Blocks`, or `Census Blocks` in 2010-based vintages).
fn block_fields(geographies: &str) -> Option<[String; 4]> {
    let layers: IndexMap<String, Vec<Map<String, Value>>> =
        serde_json::from_str(geographies).ok()?;
    let block = layers
        .get("2020 Census Blocks")
        .or_else(|| {
            layers
                .iter()
                .find(|(k, _)| k.ends_with("Census Blocks"))
                .map(|(_, v)| v)
        })?
        .first()?;
    let get = |k: &str| block.get(k)?.as_str().map(str::to_owned);
    Some([get("STATE")?, get("COUNTY")?, get("TRACT")?, get("BLOCK")?])
}

/// Build a batch Match row from a single-record `addressMatches` array. `None` when
/// the JSON cannot answer faithfully: no match, several matches (batch would likely
/// report a Tie), or no geographies when they are needed.
pub fn heuristic_from_matches(
    matches: &[u8],
    echo: &str,
    need_geographies: bool,
) -> Option<BatchView> {
    let list: Vec<MatchJson<'_>> = serde_json::from_slice(matches).ok()?;
    let [m] = list.as_slice() else {
        return None;
    };
    let [state, county, tract, block] = match (need_geographies, m.geographies) {
        (true, Some(g)) => block_fields(g.get())?.map(Some),
        (true, None) => return None,
        (false, _) => [None, None, None, None],
    };
    let exact = canonicalize_component(echo) == canonicalize_component(&m.matched_address);
    Some(BatchView {
        match_type: Some(if exact { "Exact" } else { "Non_Exact" }.to_owned()),
        matched_address: Some(m.matched_address.clone()),
        lon: Some(m.coordinates.x.get().to_owned()),
        lat: Some(m.coordinates.y.get().to_owned()),
        tiger_line_id: Some(m.tiger_line.tiger_line_id.clone()),
        side: Some(m.tiger_line.side.clone()),
        state,
        county,
        tract,
        block,
    })
}

/// Coordinates batch row fields from a stored single-record `geographies` object.
pub fn heuristic_from_geographies(geographies: &[u8]) -> Option<BatchView> {
    let [state, county, tract, block] = block_fields(std::str::from_utf8(geographies).ok()?)?;
    Some(BatchView {
        state: Some(state),
        county: Some(county),
        tract: Some(tract),
        block: Some(block),
        ..BatchView::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::envelope::UpstreamSingle;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/live/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn render_addresses(rows: &[AddressRow], parsed: &[UpstreamRow], geographies: bool) -> Vec<u8> {
        let mut w = output_writer();
        for r in rows {
            let u = parsed.iter().find(|u| u.idx == r.idx).unwrap();
            let res = match u.status {
                Status::Match => RowResult::Match(Box::new(u.view.clone())),
                Status::NoMatch => RowResult::NoMatch,
                Status::Tie => RowResult::Tie,
            };
            write_address_row(&mut w, r, &res, geographies).unwrap();
        }
        w.into_inner().unwrap()
    }

    #[test]
    fn round_trips_captured_batches_byte_for_byte() {
        let rows =
            address_rows(read_records(&fixture("batch_in.csv"), Some("batch_in.csv")).unwrap())
                .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].echo(), "zzzz nowhere, , , ");

        let geo = parse_upstream_addresses(&fixture("batch_geo.csv"));
        assert_eq!(
            render_addresses(&rows, &geo, true),
            fixture("batch_geo.csv")
        );
        // locations output is the geographies rows without the last four columns
        assert_eq!(
            render_addresses(&rows, &geo, false),
            fixture("batch_loc.csv")
        );
        let loc = parse_upstream_addresses(&fixture("batch_loc.csv"));
        assert_eq!(
            render_addresses(&rows, &loc, false),
            fixture("batch_loc.csv")
        );

        let crows = coord_rows(read_records(&fixture("coords_in.csv"), None).unwrap()).unwrap();
        let cparsed = parse_upstream_coordinates(&fixture("batch_coords.csv"));
        let mut w = output_writer();
        for r in &crows {
            let u = cparsed.iter().find(|u| u.idx == r.idx).unwrap();
            write_coord_row(&mut w, r, &RowResult::Match(Box::new(u.view.clone()))).unwrap();
        }
        assert_eq!(w.into_inner().unwrap(), fixture("batch_coords.csv"));
    }

    #[test]
    fn input_validation() {
        let recs = read_records(b"1,a,b,c,d\n\n2,a\n3,a,b,c,d,e\n", None).unwrap();
        assert_eq!(address_rows(recs), Err(BatchInputError::Malformed(vec![3])));
        let recs = read_records(b"1,a\n", None).unwrap();
        let rows = address_rows(recs).unwrap();
        assert_eq!((rows[0].street.as_str(), rows[0].zip.as_str()), ("a", ""));
        let recs = read_records(b"1,1,2\n2,x,3\n3,1\n4,inf,1\n", None).unwrap();
        assert_eq!(
            coord_rows(recs),
            Err(BatchInputError::Malformed(vec![2, 3, 4]))
        );
        let rows = address_rows(read_records(b"a,1\n\nb,2\n", None).unwrap()).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.idx).collect::<Vec<_>>(),
            [1, 2],
            "ids are record numbers (blank lines not counted)"
        );
        assert!(
            BatchInputError::Malformed((1..=60).collect())
                .to_string()
                .contains("(and 10 more)")
        );
    }

    #[test]
    fn excel_without_feature_or_garbage_is_unreadable() {
        assert!(matches!(
            read_records(b"not a workbook", Some("x.XLSX")),
            Err(BatchInputError::Unreadable(_))
        ));
    }

    #[test]
    fn upload_uses_row_numbers_as_ids() {
        let rows = address_rows(read_records(&fixture("batch_in.csv"), None).unwrap()).unwrap();
        assert_eq!(
            upload_addresses(&rows),
            b"1,4600 Silver Hill Rd,Washington,DC,20233\n2,zzzz nowhere,,,\n3,1600 Pennsylvania Ave NW,Washington,DC,20500\n"
        );
    }

    #[test]
    fn heuristic_reproduces_batch_row_from_single_record_json() {
        let json = UpstreamSingle::from_response(&fixture("geo_address_all.json"))
            .unwrap()
            .payload
            .json
            .unwrap();
        let rows = address_rows(read_records(&fixture("batch_in.csv"), None).unwrap()).unwrap();
        let v = heuristic_from_matches(&json, &rows[0].echo(), true).unwrap();
        let expected = &parse_upstream_addresses(&fixture("batch_geo.csv"))[0].view;
        assert_eq!(&v, expected);

        let without_zip = "4600 Silver Hill Rd, Washington, DC, ";
        assert_eq!(
            heuristic_from_matches(&json, without_zip, false)
                .unwrap()
                .match_type
                .as_deref(),
            Some("Non_Exact")
        );
        assert!(heuristic_from_matches(b"[]", without_zip, false).is_none());
        let two = format!(
            "[{0},{0}]",
            &std::str::from_utf8(&json).unwrap()[1..json.len() - 1]
        );
        assert!(
            heuristic_from_matches(two.as_bytes(), without_zip, false).is_none(),
            "multiple matches are not guessed"
        );

        let geo = UpstreamSingle::from_response(&fixture("geo_coords_all.json"))
            .unwrap()
            .payload
            .json
            .unwrap();
        let c = heuristic_from_geographies(&geo).unwrap();
        assert_eq!(
            c,
            parse_upstream_coordinates(&fixture("batch_coords.csv"))[0].view
        );
    }
}
