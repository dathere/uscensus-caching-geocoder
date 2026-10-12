//! Cache key grammar (PLAN.md §5.2).
//!
//! ```text
//! key := "v1" "|" searchtype "|" benchmark_id "|" vintage_id "|" canonical
//! ```
//!
//! Field order makes purge-by-benchmark a prefix scan on `v1|{st}|{bench}|`.

use std::fmt;

pub const KEY_VERSION: &str = "v1";

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub enum SearchType {
    Oneline,
    Address,
    AddressPR,
    Coordinates,
}

impl SearchType {
    pub const ALL: [Self; 4] = [
        Self::Oneline,
        Self::Address,
        Self::AddressPR,
        Self::Coordinates,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oneline => "oneline",
            Self::Address => "address",
            Self::AddressPR => "addressPR",
            Self::Coordinates => "coordinates",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

impl fmt::Display for SearchType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The lookup input, before canonicalization.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyInput {
    Oneline {
        address: String,
    },
    Address {
        street: String,
        city: String,
        state: String,
        zip: String,
    },
    AddressPR {
        street: String,
        urb: String,
        city: String,
        municipio: String,
        state: String,
        zip: String,
    },
    Coordinates {
        x: f64,
        y: f64,
    },
}

impl KeyInput {
    pub fn search_type(&self) -> SearchType {
        match self {
            Self::Oneline { .. } => SearchType::Oneline,
            Self::Address { .. } => SearchType::Address,
            Self::AddressPR { .. } => SearchType::AddressPR,
            Self::Coordinates { .. } => SearchType::Coordinates,
        }
    }

    /// The canonical segment of the key.
    pub fn canonical(&self) -> String {
        match self {
            Self::Oneline { address } => canonicalize_component(address),
            Self::Address {
                street,
                city,
                state,
                zip,
            } => join(&[street, city, state, zip]),
            Self::AddressPR {
                street,
                urb,
                city,
                municipio,
                state,
                zip,
            } => join(&[street, urb, city, municipio, state, zip]),
            Self::Coordinates { x, y } => format!("{},{}", fmt_coord(*x), fmt_coord(*y)),
        }
    }
}

fn join(parts: &[&String]) -> String {
    parts
        .iter()
        .map(|p| canonicalize_component(p))
        .collect::<Vec<_>>()
        .join(";")
}

/// Six decimal places (~11 cm), with negative zero folded to positive.
pub fn fmt_coord(v: f64) -> String {
    let s = format!("{v:.6}");
    if s == "-0.000000" {
        "0.000000".to_owned()
    } else {
        s
    }
}

/// Trim, replace `;`/`|` with space, collapse whitespace runs, ASCII-uppercase, and strip
/// trailing `.`/`,` repeatedly. Deliberately nothing more aggressive (no abbreviation
/// expansion): two inputs share an entry only when Census would see them as identical.
pub fn canonicalize_component(s: &str) -> String {
    let replaced: String = s
        .chars()
        .map(|c| if c == ';' || c == '|' { ' ' } else { c })
        .collect();
    let mut out = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    out.make_ascii_uppercase();
    loop {
        let trimmed = out.trim_end_matches(['.', ',']).trim_end();
        if trimmed.len() == out.len() {
            break;
        }
        out.truncate(trimmed.len());
    }
    out
}

/// A fully formed cache key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CacheKey(String);

/// A key split back into its fields. `canonical` is already canonical, so it can be
/// re-joined without loss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedKey {
    pub search_type: SearchType,
    pub benchmark_id: u32,
    pub vintage_id: u32,
    pub canonical: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed cache key `{0}`")]
pub struct KeyParseError(pub String);

impl CacheKey {
    pub fn new(benchmark_id: u32, vintage_id: u32, input: &KeyInput) -> Self {
        Self::from_parts(
            input.search_type(),
            benchmark_id,
            vintage_id,
            &input.canonical(),
        )
    }

    fn from_parts(st: SearchType, benchmark_id: u32, vintage_id: u32, canonical: &str) -> Self {
        Self(format!(
            "{KEY_VERSION}|{st}|{benchmark_id}|{vintage_id}|{canonical}"
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Prefix selecting every key of one searchtype on one benchmark.
    pub fn benchmark_prefix(st: SearchType, benchmark_id: u32) -> String {
        format!("{KEY_VERSION}|{st}|{benchmark_id}|")
    }

    pub fn parse(s: &str) -> Result<ParsedKey, KeyParseError> {
        let err = || KeyParseError(s.to_owned());
        let mut it = s.splitn(5, '|');
        let (Some(ver), Some(st), Some(b), Some(v), Some(canonical)) =
            (it.next(), it.next(), it.next(), it.next(), it.next())
        else {
            return Err(err());
        };
        if ver != KEY_VERSION {
            return Err(err());
        }
        let search_type = SearchType::parse(st).ok_or_else(err)?;
        let fields = canonical.split(';').count();
        let expected = match search_type {
            SearchType::Oneline | SearchType::Coordinates => 1,
            SearchType::Address => 4,
            SearchType::AddressPR => 6,
        };
        if fields != expected || canonical.contains('|') {
            return Err(err());
        }
        Ok(ParsedKey {
            search_type,
            benchmark_id: b.parse().map_err(|_| err())?,
            vintage_id: v.parse().map_err(|_| err())?,
            canonical: canonical.to_owned(),
        })
    }
}

impl From<&ParsedKey> for CacheKey {
    fn from(p: &ParsedKey) -> Self {
        Self::from_parts(p.search_type, p.benchmark_id, p.vintage_id, &p.canonical)
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(street: &str, city: &str, state: &str, zip: &str) -> KeyInput {
        KeyInput::Address {
            street: street.into(),
            city: city.into(),
            state: state.into(),
            zip: zip.into(),
        }
    }

    #[test]
    fn canonicalize_component_rules() {
        assert_eq!(
            canonicalize_component("  4600  Silver\tHill Rd. "),
            "4600 SILVER HILL RD"
        );
        assert_eq!(canonicalize_component("Main St.,.,"), "MAIN ST");
        assert_eq!(canonicalize_component("Main St . ,"), "MAIN ST");
        assert_eq!(canonicalize_component("a;b|c"), "A B C");
        assert_eq!(canonicalize_component("St. Paul"), "ST. PAUL");
        assert_eq!(canonicalize_component("Peña Blvd"), "PEñA BLVD");
        assert_eq!(canonicalize_component(""), "");
        assert_eq!(canonicalize_component(" ., "), "");
    }

    #[test]
    fn address_key_grammar() {
        let k = CacheKey::new(4, 4, &addr("4600 Silver Hill Rd", "Washington", "dc", ""));
        assert_eq!(
            k.as_str(),
            "v1|address|4|4|4600 SILVER HILL RD;WASHINGTON;DC;"
        );
        let pr = KeyInput::AddressPR {
            street: "1 Calle A".into(),
            urb: "".into(),
            city: "San Juan".into(),
            municipio: "".into(),
            state: "PR".into(),
            zip: "00901".into(),
        };
        assert_eq!(
            CacheKey::new(4, 4, &pr).as_str(),
            "v1|addressPR|4|4|1 CALLE A;;SAN JUAN;;PR;00901"
        );
        let one = KeyInput::Oneline {
            address: "4600 Silver Hill Rd, Washington, DC 20233".into(),
        };
        assert_eq!(
            CacheKey::new(4, 4, &one).as_str(),
            "v1|oneline|4|4|4600 SILVER HILL RD, WASHINGTON, DC 20233"
        );
    }

    #[test]
    fn equivalent_inputs_share_a_key() {
        let a = CacheKey::new(
            4,
            4,
            &addr("4600 silver hill rd.", " Washington ", "DC", "20233"),
        );
        let b = CacheKey::new(
            4,
            4,
            &addr("4600 SILVER  HILL RD", "WASHINGTON", "dc", "20233"),
        );
        assert_eq!(a, b);
    }

    #[test]
    fn coordinate_rounding() {
        let k = |x, y| CacheKey::new(4, 4, &KeyInput::Coordinates { x, y });
        assert_eq!(
            k(-76.92744, 38.84601).as_str(),
            "v1|coordinates|4|4|-76.927440,38.846010"
        );
        assert_eq!(k(-76.9274404, 38.8460096), k(-76.92744, 38.84601));
        assert_eq!(
            k(-0.0000001, 0.0).as_str(),
            "v1|coordinates|4|4|0.000000,0.000000"
        );
        assert_eq!(
            k(-0.0, -0.0).as_str(),
            "v1|coordinates|4|4|0.000000,0.000000"
        );
        assert_ne!(k(-76.9274406, 1.0), k(-76.9274394, 1.0));
    }

    #[test]
    fn parse_round_trips() {
        let inputs = [
            addr("4600 Silver Hill Rd", "Washington", "DC", "20233"),
            addr("", "", "", ""),
            KeyInput::Oneline {
                address: "x; y".into(),
            },
            KeyInput::Coordinates {
                x: -77.0365,
                y: 38.8977,
            },
        ];
        for input in &inputs {
            let k = CacheKey::new(2020, 2010, input);
            let p = CacheKey::parse(k.as_str()).unwrap();
            assert_eq!(p.search_type, input.search_type());
            assert_eq!((p.benchmark_id, p.vintage_id), (2020, 2010));
            assert_eq!(CacheKey::from(&p), k);
        }
    }

    #[test]
    fn parse_rejects_malformed() {
        for bad in [
            "",
            "v2|address|4|4|A;B;C;D",
            "v1|street|4|4|A",
            "v1|address|x|4|A;B;C;D",
            "v1|address|4|4|A;B;C",
            "v1|oneline|4|4|A;B",
            "v1|oneline|4|4|A|B",
            "v1|oneline|4|4",
        ] {
            assert!(CacheKey::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn benchmark_prefix_matches_keys() {
        let k = CacheKey::new(4, 426, &addr("1 Main St", "X", "Y", "Z"));
        assert!(
            k.as_str()
                .starts_with(&CacheKey::benchmark_prefix(SearchType::Address, 4))
        );
        assert!(
            !k.as_str()
                .starts_with(&CacheKey::benchmark_prefix(SearchType::Address, 42))
        );
    }
}
