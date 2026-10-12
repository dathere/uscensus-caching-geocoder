//! Query parameter parsing for single-record requests.
//!
//! Only errors whose exact upstream text is known (captured in `tests/fixtures/live`)
//! are answered locally. Anything else we cannot turn into a cache key (missing street,
//! bad coordinates, unknown benchmark, non-JSON `format`, ...) is a
//! [`Parsed::Passthrough`]: the request goes upstream uncached and the answer is
//! forwarded verbatim, so error texts and HTML output stay exactly Census's.

use super::types::{ReturnType, SearchPath};
use crate::{cache::key::KeyInput, error::AppError};

pub const MSG_NO_BENCHMARK: &str = "Benchmark must be specified";
pub const MSG_NO_VINTAGE: &str = "Vintage must be specified";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Format {
    Json,
    Jsonp(String),
}

/// A validated request that can be served from the cache.
#[derive(Debug, Clone, PartialEq)]
pub struct GeocodeRequest {
    pub returntype: ReturnType,
    pub search: SearchPath,
    pub input: KeyInput,
    /// Non-empty address params as given, in Census's echo order (for `result.input`).
    pub echo: Vec<(&'static str, String)>,
    /// Raw `x`/`y` for the coordinates echo.
    pub location: Option<(f64, f64)>,
    pub benchmark: String,
    pub vintage: Option<String>,
    pub layers: Option<String>,
    pub format: Format,
    /// `flat=1` / `X-UCG-Flatten: 1` extension.
    pub flat: bool,
}

#[derive(Debug)]
pub enum Parsed {
    Request(Box<GeocodeRequest>),
    /// Answer locally with a Census-shaped error.
    Error(AppError),
    /// Forward the original request upstream uncached.
    Passthrough,
}

fn get<'a>(pairs: &'a [(String, String)], k: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(key, _)| key == k)
        .map(|(_, v)| v.as_str())
}

fn present<'a>(pairs: &'a [(String, String)], k: &str) -> Option<&'a str> {
    get(pairs, k).filter(|v| !v.trim().is_empty())
}

/// JSONP callbacks are reflected into a script body, so only plain identifiers pass.
pub fn valid_callback(cb: &str) -> bool {
    let mut chars = cb.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.')
}

pub fn parse(
    rt: ReturnType,
    search: SearchPath,
    pairs: &[(String, String)],
    flat_header: bool,
) -> Parsed {
    let format = match get(pairs, "format").map(str::trim) {
        Some(f) if f.eq_ignore_ascii_case("json") => Format::Json,
        Some(f) if f.eq_ignore_ascii_case("jsonp") => match get(pairs, "callback") {
            Some(cb) if valid_callback(cb) => Format::Jsonp(cb.to_owned()),
            _ => {
                return Parsed::Error(AppError::BadRequest(
                    "Invalid or missing JSONP callback".into(),
                ));
            }
        },
        // Census defaults to HTML; we do not render it.
        _ => return Parsed::Passthrough,
    };
    let Some(benchmark) = present(pairs, "benchmark") else {
        return Parsed::Error(AppError::BadRequest(MSG_NO_BENCHMARK.into()));
    };
    let vintage = present(pairs, "vintage");
    if rt == ReturnType::Geographies && vintage.is_none() {
        return Parsed::Error(AppError::BadRequest(MSG_NO_VINTAGE.into()));
    }
    if rt == ReturnType::Locations && search == SearchPath::Coordinates {
        return Parsed::Passthrough;
    }

    let s = |k| get(pairs, k).unwrap_or_default().to_owned();
    let mut echo = Vec::new();
    let mut location = None;
    let input = match search {
        SearchPath::OnelineAddress => {
            let Some(a) = present(pairs, "address") else {
                return Parsed::Passthrough;
            };
            echo.push(("address", a.to_owned()));
            KeyInput::Oneline {
                address: a.to_owned(),
            }
        }
        SearchPath::Address => {
            if present(pairs, "street").is_none() {
                return Parsed::Passthrough;
            }
            for k in ["zip", "city", "street", "state"] {
                if let Some(v) = present(pairs, k) {
                    echo.push((k, v.to_owned()));
                }
            }
            KeyInput::Address {
                street: s("street"),
                city: s("city"),
                state: s("state"),
                zip: s("zip"),
            }
        }
        SearchPath::AddressPR => {
            if present(pairs, "street").is_none() {
                return Parsed::Passthrough;
            }
            for k in ["zip", "city", "municipio", "street", "urb", "state"] {
                if let Some(v) = present(pairs, k) {
                    echo.push((k, v.to_owned()));
                }
            }
            KeyInput::AddressPR {
                street: s("street"),
                urb: s("urb"),
                city: s("city"),
                municipio: s("municipio"),
                state: s("state"),
                zip: s("zip"),
            }
        }
        SearchPath::Coordinates => {
            let num = |k| {
                present(pairs, k)
                    .and_then(|v| v.trim().parse::<f64>().ok())
                    .filter(|v| v.is_finite())
            };
            let (Some(x), Some(y)) = (num("x"), num("y")) else {
                return Parsed::Passthrough;
            };
            location = Some((x, y));
            KeyInput::Coordinates { x, y }
        }
    };

    let flat = flat_header || get(pairs, "flat").is_some_and(|v| matches!(v.trim(), "1" | "true"));
    Parsed::Request(Box::new(GeocodeRequest {
        returntype: rt,
        search,
        input,
        echo,
        location,
        benchmark: benchmark.trim().to_owned(),
        vintage: if rt == ReturnType::Geographies {
            vintage.map(|v| v.trim().to_owned())
        } else {
            None
        },
        layers: present(pairs, "layers").map(str::to_owned),
        format,
        flat,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> Vec<(String, String)> {
        form_urlencoded::parse(s.as_bytes()).into_owned().collect()
    }

    fn req(rt: ReturnType, st: SearchPath, s: &str) -> GeocodeRequest {
        match parse(rt, st, &q(s), false) {
            Parsed::Request(r) => *r,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn known_errors_are_local() {
        let p = parse(
            ReturnType::Locations,
            SearchPath::OnelineAddress,
            &q("address=x&format=json"),
            false,
        );
        assert!(matches!(p, Parsed::Error(AppError::BadRequest(m)) if m == MSG_NO_BENCHMARK));
        let p = parse(
            ReturnType::Geographies,
            SearchPath::Address,
            &q("street=1+Main&benchmark=4&format=json"),
            false,
        );
        assert!(matches!(p, Parsed::Error(AppError::BadRequest(m)) if m == MSG_NO_VINTAGE));
        let p = parse(
            ReturnType::Locations,
            SearchPath::OnelineAddress,
            &q("address=x&benchmark=4&format=jsonp&callback=alert(1)"),
            false,
        );
        assert!(matches!(p, Parsed::Error(_)));
    }

    #[test]
    fn unknown_cases_pass_through() {
        for (rt, st, s) in [
            (
                ReturnType::Locations,
                SearchPath::OnelineAddress,
                "address=x&benchmark=4",
            ),
            (
                ReturnType::Locations,
                SearchPath::OnelineAddress,
                "address=x&benchmark=4&format=html",
            ),
            (
                ReturnType::Locations,
                SearchPath::Address,
                "city=X&benchmark=4&format=json",
            ),
            (
                ReturnType::Locations,
                SearchPath::OnelineAddress,
                "address=+&benchmark=4&format=json",
            ),
            (
                ReturnType::Locations,
                SearchPath::Coordinates,
                "x=1&y=2&benchmark=4&format=json",
            ),
            (
                ReturnType::Geographies,
                SearchPath::Coordinates,
                "x=abc&y=2&benchmark=4&vintage=4&format=json",
            ),
            (
                ReturnType::Geographies,
                SearchPath::Coordinates,
                "x=NaN&y=2&benchmark=4&vintage=4&format=json",
            ),
        ] {
            assert!(
                matches!(parse(rt, st, &q(s), false), Parsed::Passthrough),
                "{s}"
            );
        }
    }

    #[test]
    fn address_echo_order_and_values() {
        let r = req(
            ReturnType::Locations,
            SearchPath::Address,
            "street=4600+Silver+Hill+Rd&city=Washington&state=DC&zip=20233&benchmark=Public_AR_Current&format=json",
        );
        assert_eq!(
            r.echo.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            ["zip", "city", "street", "state"]
        );
        assert_eq!(r.benchmark, "Public_AR_Current");
        assert_eq!(r.vintage, None);
        assert_eq!(r.format, Format::Json);
        assert!(!r.flat);
    }

    #[test]
    fn coordinates_jsonp_flat() {
        let r = req(
            ReturnType::Geographies,
            SearchPath::Coordinates,
            "x=-76.92744&y=38.84601&benchmark=4&vintage=4&layers=8,12&format=jsonp&callback=cb.x_1&flat=1",
        );
        assert_eq!(r.location, Some((-76.92744, 38.84601)));
        assert_eq!(r.layers.as_deref(), Some("8,12"));
        assert_eq!(r.format, Format::Jsonp("cb.x_1".into()));
        assert!(r.flat);
    }

    #[test]
    fn callbacks() {
        for ok in ["test", "_x", "$", "a.b.c", "jQuery123_456"] {
            assert!(valid_callback(ok), "{ok}");
        }
        for bad in ["", "1a", "a(b)", "a;b", "a b", "<script>"] {
            assert!(!valid_callback(bad), "{bad}");
        }
    }
}
