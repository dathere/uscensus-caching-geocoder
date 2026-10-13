//! Assemble single-record responses from a stored entry.
//!
//! `result.input` is always rebuilt from the request and the catalog's raw
//! benchmark/vintage objects. The `addressMatches` / `geographies` segments come from the
//! stored upstream bytes: untouched for `layers=all`, otherwise re-assembled around the
//! original raw values (see [`super::layers`]).

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Number, Value, value::RawValue};

use super::{
    layers::{LayerFilter, filter_geographies},
    params::{Format, GeocodeRequest},
    types::{ReturnType, SearchPath},
};

pub const CT_JSON: &str = "application/json";
pub const CT_JSONP: &str = "application/javascript";

/// Everything a response needs besides the request.
pub struct RenderInput<'a> {
    /// Stored raw JSON: `addressMatches` array or coordinates `geographies` object;
    /// `None` renders an empty match list (NoMatch/Tie without a JSON payload).
    pub stored: Option<&'a [u8]>,
    pub benchmark: &'a Map<String, Value>,
    /// Echoed for `geographies` requests only.
    pub vintage: Option<&'a Map<String, Value>>,
    pub layers: &'a LayerFilter,
}

#[derive(Serialize)]
struct Out<R: Serialize> {
    result: R,
}

#[derive(Serialize)]
struct AddressResult<'a> {
    input: &'a Map<String, Value>,
    #[serde(rename = "addressMatches")]
    address_matches: &'a RawValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    flat: Option<Value>,
}

#[derive(Serialize)]
struct CoordinatesResult<'a> {
    geographies: &'a RawValue,
    input: &'a Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    flat: Option<Value>,
}

fn coord_number(v: f64) -> Value {
    Number::from_f64(v).map_or(Value::Null, Value::Number)
}

fn build_input(req: &GeocodeRequest, r: &RenderInput<'_>) -> Map<String, Value> {
    let mut input = Map::new();
    if req.search == SearchPath::Coordinates {
        if let Some(v) = r.vintage {
            input.insert("vintage".into(), Value::Object(v.clone()));
        }
        if let Some((x, y)) = req.location {
            let mut loc = Map::new();
            loc.insert("x".into(), coord_number(x));
            loc.insert("y".into(), coord_number(y));
            input.insert("location".into(), Value::Object(loc));
        }
    } else {
        let address: Map<String, Value> = req
            .echo
            .iter()
            .map(|(k, v)| ((*k).to_owned(), Value::String(v.clone())))
            .collect();
        input.insert("address".into(), Value::Object(address));
        if req.returntype == ReturnType::Geographies
            && let Some(v) = r.vintage
        {
            input.insert("vintage".into(), Value::Object(v.clone()));
        }
    }
    input.insert("benchmark".into(), Value::Object(r.benchmark.clone()));
    input
}

/// `flat`: `{"<layer>": {"<attr>": <scalar>, ...}}` from the first element of each
/// layer (qsv's `json_dotpath` cannot index arrays; a nested object keeps every path
/// dot-separated).
fn flatten(geographies: &RawValue) -> serde_json::Result<Value> {
    let layers: IndexMap<String, Vec<Map<String, Value>>> =
        serde_json::from_str(geographies.get())?;
    let mut flat = Map::new();
    for (name, items) in layers {
        if let Some(first) = items.into_iter().next() {
            let attrs: Map<String, Value> = first
                .into_iter()
                .filter(|(_, v)| !v.is_object() && !v.is_array())
                .collect();
            flat.insert(name, Value::Object(attrs));
        }
    }
    Ok(Value::Object(flat))
}

/// Project stored `addressMatches` for the request: drop `geographies` for `locations`,
/// filter layers for `geographies`. Returns the raw array plus the first match's
/// (filtered) geographies for `flat`.
fn project_matches(
    stored: &[u8],
    rt: ReturnType,
    layers: &LayerFilter,
) -> serde_json::Result<(Box<RawValue>, Option<Box<RawValue>>)> {
    let text = std::str::from_utf8(stored).map_err(serde::de::Error::custom)?;
    let mut matches: Vec<IndexMap<String, Box<RawValue>>> = serde_json::from_str(text)?;
    let mut first_geo = None;
    let untouched = rt == ReturnType::Geographies && *layers == LayerFilter::All;
    for (i, m) in matches.iter_mut().enumerate() {
        match rt {
            ReturnType::Locations => {
                m.shift_remove("geographies");
            }
            ReturnType::Geographies => {
                if let Some(g) = m.get_mut("geographies") {
                    if !untouched {
                        *g = filter_geographies(g, layers)?;
                    }
                    if i == 0 {
                        first_geo = Some(g.clone());
                    }
                }
            }
        }
    }
    let raw = if untouched {
        RawValue::from_string(text.to_owned())?
    } else {
        serde_json::value::to_raw_value(&matches)?
    };
    Ok((raw, first_geo))
}

/// Render the response body (JSON, or JSONP wrapped as `/**/cb(<json>);`) and its
/// Content-Type.
pub fn render(
    req: &GeocodeRequest,
    r: &RenderInput<'_>,
) -> serde_json::Result<(Vec<u8>, &'static str)> {
    let input = build_input(req, r);
    let json = if req.search == SearchPath::Coordinates {
        let stored = r.stored.unwrap_or(b"{}");
        let raw: Box<RawValue> = serde_json::from_slice(stored)?;
        let geo = filter_geographies(&raw, r.layers)?;
        let flat = if req.flat { Some(flatten(&geo)?) } else { None };
        serde_json::to_vec(&Out {
            result: CoordinatesResult {
                geographies: &geo,
                input: &input,
                flat,
            },
        })?
    } else {
        let (matches, first_geo) = match r.stored {
            Some(s) => project_matches(s, req.returntype, r.layers)?,
            None => (RawValue::from_string("[]".into())?, None),
        };
        let flat = match (req.flat, first_geo) {
            (true, Some(g)) => Some(flatten(&g)?),
            (true, None) => Some(Value::Object(Map::new())),
            (false, _) => None,
        };
        serde_json::to_vec(&Out {
            result: AddressResult {
                input: &input,
                address_matches: &matches,
                flat,
            },
        })?
    };
    Ok(match &req.format {
        Format::Json => (json, CT_JSON),
        Format::Jsonp(cb) => {
            let mut out = Vec::with_capacity(json.len() + cb.len() + 8);
            out.extend_from_slice(b"/**/");
            out.extend_from_slice(cb.as_bytes());
            out.push(b'(');
            out.extend_from_slice(&json);
            out.extend_from_slice(b");");
            (out, CT_JSONP)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cache::envelope::UpstreamSingle,
        catalog::Catalog,
        census::params::{Parsed, parse},
    };

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/live/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn request(rt: ReturnType, st: SearchPath, q: &str) -> GeocodeRequest {
        let pairs: Vec<(String, String)> =
            form_urlencoded::parse(q.as_bytes()).into_owned().collect();
        match parse(rt, st, &pairs, false) {
            Parsed::Request(r) => *r,
            other => panic!("{other:?}"),
        }
    }

    fn value(b: &[u8]) -> Value {
        serde_json::from_slice(b).unwrap()
    }

    #[test]
    fn locations_projection_is_byte_identical_to_upstream() {
        let stored = UpstreamSingle::from_response(&fixture("geo_address_all.json"))
            .unwrap()
            .payload
            .json
            .unwrap();
        let c = Catalog::snapshot();
        let req = request(
            ReturnType::Locations,
            SearchPath::Address,
            "street=4600+Silver+Hill+Rd&city=Washington&state=DC&zip=20233&benchmark=Public_AR_Current&format=json",
        );
        let (body, ct) = render(
            &req,
            &RenderInput {
                stored: Some(&stored),
                benchmark: &c.benchmark(4).unwrap().raw,
                vintage: None,
                layers: &LayerFilter::All,
            },
        )
        .unwrap();
        assert_eq!(ct, CT_JSON);
        let expected = fixture("loc_address.json");
        assert_eq!(value(&body), value(&expected));
        assert_eq!(
            body,
            expected.trim_ascii_end(),
            "whole body is byte-identical"
        );
    }

    #[test]
    fn jsonp_wrapping() {
        let stored = UpstreamSingle::from_response(&fixture("geo_address_all.json"))
            .unwrap()
            .payload
            .json
            .unwrap();
        let c = Catalog::snapshot();
        let req = request(
            ReturnType::Locations,
            SearchPath::OnelineAddress,
            "address=4600+Silver+Hill+Rd%2C+Washington%2C+DC+20233&benchmark=4&format=jsonp&callback=test",
        );
        let (body, ct) = render(
            &req,
            &RenderInput {
                stored: Some(&stored),
                benchmark: &c.benchmark(4).unwrap().raw,
                vintage: None,
                layers: &LayerFilter::All,
            },
        )
        .unwrap();
        assert_eq!(ct, CT_JSONP);
        assert_eq!(body, fixture("loc_jsonp.js").trim_ascii_end());
    }

    #[test]
    fn flat_extension_nests_first_element_scalars() {
        let stored = UpstreamSingle::from_response(&fixture("geo_coords_all.json"))
            .unwrap()
            .payload
            .json
            .unwrap();
        let c = Catalog::snapshot();
        let req = request(
            ReturnType::Geographies,
            SearchPath::Coordinates,
            "x=-76.92744&y=38.84601&benchmark=4&vintage=4&layers=8&format=json&flat=1",
        );
        let layers = LayerFilter::resolve(req.layers.as_deref(), "Current_Current", &c);
        let (body, _) = render(
            &req,
            &RenderInput {
                stored: Some(&stored),
                benchmark: &c.benchmark(4).unwrap().raw,
                vintage: Some(&c.vintage(4, 4).unwrap().raw),
                layers: &layers,
            },
        )
        .unwrap();
        let v = value(&body);
        assert_eq!(v["result"]["flat"]["Census Tracts"]["GEOID"], "24033802405");
        assert_eq!(v["result"]["geographies"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn nomatch_without_payload_renders_empty_matches() {
        let c = Catalog::snapshot();
        let req = request(
            ReturnType::Locations,
            SearchPath::OnelineAddress,
            "address=zzzz+nowhere&benchmark=4&format=json",
        );
        let (body, _) = render(
            &req,
            &RenderInput {
                stored: None,
                benchmark: &c.benchmark(4).unwrap().raw,
                vintage: None,
                layers: &LayerFilter::All,
            },
        )
        .unwrap();
        assert_eq!(body, fixture("loc_nomatch.json").trim_ascii_end());
    }
}
