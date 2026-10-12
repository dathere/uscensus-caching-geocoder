//! Entry envelope (PLAN.md §5.3) and the cached-compatible stored value.
//!
//! Stored value (redb `cached_disk_cache`, as written by `cached::RedbCache`):
//! `rmp_serde::to_vec(&(bin(envelope), created_at: SystemTime))`, a positional pair.
//!
//! Envelope = 17-byte header + payload:
//!
//! | Off | Size | Field |
//! |---|---|---|
//! | 0 | 1 | `fmt_ver` = 1 |
//! | 1 | 4 | `dict_id` u32 LE (0 = none) |
//! | 5 | 1 | `codec`: 0 raw, 1 zstd+dict, 2 zstd |
//! | 6 | 1 | `status`: 0 Match, 1 NoMatch, 2 Tie |
//! | 7 | 1 | `source`: 0 Single, 1 Batch, 2 Job, 3 Import, 4 Confirmed (re-checked NoMatch) |
//! | 8 | 1 | `views` bitflags: 0x01 JSON, 0x02 GEO, 0x04 BATCH |
//! | 9 | 8 | `verified_at` u64 LE unix seconds |
//! | 17 | n | payload: `rmp_serde::to_vec_named(EntryPayload)`, compressed per `codec` |

use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use serde_json::value::RawValue;

use super::codec::{Codec, CodecError, CodecKind};

pub const FMT_VER: u8 = 1;
pub const HEADER_LEN: usize = 17;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[repr(u8)]
pub enum Status {
    Match = 0,
    NoMatch = 1,
    Tie = 2,
}

impl Status {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Match),
            1 => Some(Self::NoMatch),
            2 => Some(Self::Tie),
            _ => None,
        }
    }
}

/// How an entry was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[repr(u8)]
pub enum EntrySource {
    Single = 0,
    Batch = 1,
    Job = 2,
    Import = 3,
    /// A batch/job NoMatch re-checked `retry.negative_attempts` times by the
    /// refresh-negatives job: kept for `ttl.nomatch_single`, no further retries.
    Confirmed = 4,
}

impl EntrySource {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Single),
            1 => Some(Self::Batch),
            2 => Some(Self::Job),
            3 => Some(Self::Import),
            4 => Some(Self::Confirmed),
            _ => None,
        }
    }
}

/// Which response shapes an entry can serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Views(u8);

impl Views {
    /// `addressMatches` (single-record locations JSON).
    pub const JSON: Self = Self(0x01);
    /// `geographies` (all layers).
    pub const GEO: Self = Self(0x02);
    /// Batch CSV row fields.
    pub const BATCH: Self = Self(0x04);

    pub const fn bits(self) -> u8 {
        self.0
    }
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & 0x07)
    }
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::ops::BitOr for Views {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// Batch CSV fields for one row. Strings are kept exactly as upstream wrote them.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BatchView {
    pub match_type: Option<String>,
    pub matched_address: Option<String>,
    pub lon: Option<String>,
    pub lat: Option<String>,
    pub tiger_line_id: Option<String>,
    pub side: Option<String>,
    pub state: Option<String>,
    pub county: Option<String>,
    pub tract: Option<String>,
    pub block: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EntryPayload {
    /// Raw upstream bytes: the `addressMatches` array (address searches) or the
    /// `geographies` object (coordinates), byte-exact.
    pub json: Option<ByteBuf>,
    pub batch: Option<BatchView>,
    pub benchmark_name: String,
    pub vintage_name: String,
}

/// A decoded cache entry (without `created_at`, which lives in the stored value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub status: Status,
    pub source: EntrySource,
    pub views: Views,
    pub verified_at: u64,
    pub payload: EntryPayload,
}

/// The fixed-size header, readable without decompressing the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub fmt_ver: u8,
    pub dict_id: u32,
    pub codec: CodecKind,
    pub status: Status,
    pub source: EntrySource,
    pub views: Views,
    pub verified_at: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("envelope shorter than its {HEADER_LEN}-byte header")]
    TooShort,
    #[error("unsupported envelope format version {0}")]
    UnsupportedVersion(u8),
    #[error("invalid envelope header field `{0}`")]
    BadHeader(&'static str),
    #[error("entry uses unknown dictionary id {0}")]
    UnknownDict(u32),
    #[error("codec: {0}")]
    Codec(CodecError),
    #[error("payload: {0}")]
    Payload(String),
    #[error("stored value: {0}")]
    Stored(String),
}

impl From<CodecError> for EnvelopeError {
    fn from(e: CodecError) -> Self {
        match e {
            CodecError::UnknownDict(id) => Self::UnknownDict(id),
            other => Self::Codec(other),
        }
    }
}

impl Header {
    pub fn parse(bytes: &[u8]) -> Result<Self, EnvelopeError> {
        let h = bytes.get(..HEADER_LEN).ok_or(EnvelopeError::TooShort)?;
        if h[0] != FMT_VER {
            return Err(EnvelopeError::UnsupportedVersion(h[0]));
        }
        Ok(Self {
            fmt_ver: h[0],
            dict_id: u32::from_le_bytes(h[1..5].try_into().expect("4 bytes")),
            codec: CodecKind::from_u8(h[5]).ok_or(EnvelopeError::BadHeader("codec"))?,
            status: Status::from_u8(h[6]).ok_or(EnvelopeError::BadHeader("status"))?,
            source: EntrySource::from_u8(h[7]).ok_or(EnvelopeError::BadHeader("source"))?,
            views: Views::from_bits(h[8]),
            verified_at: u64::from_le_bytes(h[9..17].try_into().expect("8 bytes")),
        })
    }
}

impl Entry {
    pub fn encode(&self, codec: &Codec) -> Result<Vec<u8>, EnvelopeError> {
        let raw = rmp_serde::to_vec_named(&self.payload)
            .map_err(|e| EnvelopeError::Payload(e.to_string()))?;
        let (kind, dict_id, body) = codec.compress(&raw)?;
        let mut out = Vec::with_capacity(HEADER_LEN + body.len());
        out.push(FMT_VER);
        out.extend_from_slice(&dict_id.to_le_bytes());
        out.push(kind as u8);
        out.push(self.status as u8);
        out.push(self.source as u8);
        out.push(self.views.bits());
        out.extend_from_slice(&self.verified_at.to_le_bytes());
        out.extend_from_slice(&body);
        Ok(out)
    }

    pub fn decode(bytes: &[u8], codec: &Codec) -> Result<Self, EnvelopeError> {
        let h = Header::parse(bytes)?;
        let raw = codec.decompress(h.codec, h.dict_id, &bytes[HEADER_LEN..])?;
        let payload =
            rmp_serde::from_slice(&raw).map_err(|e| EnvelopeError::Payload(e.to_string()))?;
        Ok(Self {
            status: h.status,
            source: h.source,
            views: h.views,
            verified_at: h.verified_at,
            payload,
        })
    }
}

/// Wrap envelope bytes as a `cached::RedbCache` value: positional `[bin, created_at]`.
pub fn encode_stored(envelope: &[u8], created_at: SystemTime) -> Vec<u8> {
    rmp_serde::to_vec(&(serde_bytes::Bytes::new(envelope), created_at))
        .expect("bytes and SystemTime always serialize")
}

/// Inverse of [`encode_stored`]. Also accepts values written by `cached` itself with a
/// `Vec<u8>` value type (a msgpack array of integers instead of bin).
pub fn decode_stored(bytes: &[u8]) -> Result<(Vec<u8>, SystemTime), EnvelopeError> {
    let (env, created_at): (ByteBuf, SystemTime) =
        rmp_serde::from_slice(bytes).map_err(|e| EnvelopeError::Stored(e.to_string()))?;
    Ok((env.into_vec(), created_at))
}

/// What a single-record upstream response yields for storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamSingle {
    pub status: Status,
    pub views: Views,
    pub payload: EntryPayload,
}

#[derive(Deserialize)]
struct Resp<'a> {
    #[serde(borrow)]
    result: RespResult<'a>,
}

#[derive(Deserialize)]
struct RespResult<'a> {
    #[serde(borrow, rename = "addressMatches")]
    address_matches: Option<&'a RawValue>,
    #[serde(borrow)]
    geographies: Option<&'a RawValue>,
    input: Option<RespInput>,
}

#[derive(Deserialize)]
struct RespInput {
    benchmark: Option<NamedBenchmark>,
    vintage: Option<NamedVintage>,
}

#[derive(Deserialize)]
struct NamedBenchmark {
    #[serde(rename = "benchmarkName")]
    name: String,
}

#[derive(Deserialize)]
struct NamedVintage {
    #[serde(rename = "vintageName")]
    name: String,
}

#[derive(Deserialize)]
struct MatchProbe<'a> {
    #[serde(borrow)]
    geographies: Option<&'a RawValue>,
}

impl UpstreamSingle {
    /// Extract the storable parts of a 200 single-record JSON response
    /// (`locations`/`geographies` for addresses, or `geographies/coordinates`).
    pub fn from_response(body: &[u8]) -> Result<Self, EnvelopeError> {
        let bad = |e: serde_json::Error| EnvelopeError::Payload(e.to_string());
        let resp: Resp<'_> = serde_json::from_slice(body).map_err(bad)?;
        let r = resp.result;
        let (benchmark_name, vintage_name) = r
            .input
            .map(|i| {
                (
                    i.benchmark.map(|b| b.name).unwrap_or_default(),
                    i.vintage.map(|v| v.name).unwrap_or_default(),
                )
            })
            .unwrap_or_default();

        let (status, views, json) = if let Some(matches) = r.address_matches {
            let list: Vec<&RawValue> = serde_json::from_str(matches.get()).map_err(bad)?;
            let mut views = Views::JSON;
            if let Some(first) = list.first() {
                let probe: MatchProbe<'_> = serde_json::from_str(first.get()).map_err(bad)?;
                if probe.geographies.is_some() {
                    views = views | Views::GEO;
                }
            }
            let status = if list.is_empty() {
                Status::NoMatch
            } else {
                Status::Match
            };
            (status, views, matches.get())
        } else if let Some(geo) = r.geographies {
            let layers: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(geo.get()).map_err(bad)?;
            let any = layers
                .values()
                .any(|v| v.as_array().is_some_and(|a| !a.is_empty()));
            let status = if any { Status::Match } else { Status::NoMatch };
            (status, Views::GEO, geo.get())
        } else {
            return Err(EnvelopeError::Payload(
                "response has neither addressMatches nor geographies".into(),
            ));
        };

        Ok(Self {
            status,
            views,
            payload: EntryPayload {
                json: Some(ByteBuf::from(json.as_bytes())),
                batch: None,
                benchmark_name,
                vintage_name,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::codec::{Dictionary, tests::test_dict};

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/live/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn sample_entry() -> Entry {
        Entry {
            status: Status::NoMatch,
            source: EntrySource::Batch,
            views: Views::BATCH,
            verified_at: 0x0102_0304_0506_0708,
            payload: EntryPayload {
                json: None,
                batch: None,
                benchmark_name: "B".into(),
                vintage_name: "V".into(),
            },
        }
    }

    #[test]
    fn exact_byte_layout() {
        let e = sample_entry();
        let bytes = e.encode(&Codec::default()).unwrap();
        let payload = rmp_serde::to_vec_named(&e.payload).unwrap();
        let mut expected = vec![
            1, // fmt_ver
            0, 0, 0, 0,    // dict_id
            0,    // codec raw (payload too small for zstd to win)
            1,    // NoMatch
            1,    // Batch
            0x04, // views BATCH
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // verified_at LE
        ];
        expected.extend_from_slice(&payload);
        assert_eq!(bytes, expected);
        assert_eq!(Entry::decode(&bytes, &Codec::default()).unwrap(), e);
    }

    #[test]
    fn dict_compressed_round_trip_and_header() {
        let codec = Codec::new(Some(Dictionary::new(&test_dict()).unwrap()), vec![]);
        let single = UpstreamSingle::from_response(&fixture("geo_address_all.json")).unwrap();
        let e = Entry {
            status: single.status,
            source: EntrySource::Single,
            views: single.views,
            verified_at: 1_760_000_000,
            payload: single.payload,
        };
        let bytes = e.encode(&codec).unwrap();
        let h = Header::parse(&bytes).unwrap();
        assert_eq!(h.codec, CodecKind::ZstdDict);
        assert_eq!(h.dict_id, codec.current_dict_id());
        assert_eq!(
            (h.status, h.source, h.views),
            (Status::Match, EntrySource::Single, Views::JSON | Views::GEO)
        );
        assert_eq!(Entry::decode(&bytes, &codec).unwrap(), e);

        let err = Entry::decode(&bytes, &Codec::default()).unwrap_err();
        assert!(matches!(err, EnvelopeError::UnknownDict(id) if id == h.dict_id));
    }

    #[test]
    fn header_validation() {
        assert!(matches!(
            Header::parse(&[1, 0, 0]),
            Err(EnvelopeError::TooShort)
        ));
        let mut bytes = sample_entry().encode(&Codec::default()).unwrap();
        bytes[0] = 2;
        assert!(matches!(
            Header::parse(&bytes),
            Err(EnvelopeError::UnsupportedVersion(2))
        ));
        bytes[0] = 1;
        bytes[6] = 9;
        assert!(matches!(
            Header::parse(&bytes),
            Err(EnvelopeError::BadHeader("status"))
        ));
    }

    #[test]
    fn stored_value_round_trip() {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_760_000_000);
        let stored = encode_stored(b"\x01abc", t);
        assert_eq!(
            &stored[..3],
            &[0x92, 0xc4, 4],
            "positional array of [bin, created_at]"
        );
        assert_eq!(decode_stored(&stored).unwrap(), (b"\x01abc".to_vec(), t));

        let as_int_array = rmp_serde::to_vec(&(vec![1u8, 2, 200], t)).unwrap();
        assert_eq!(decode_stored(&as_int_array).unwrap(), (vec![1, 2, 200], t));
    }

    fn assert_passthrough(fixture_bytes: &[u8], json: &[u8]) {
        assert!(
            fixture_bytes.windows(json.len()).any(|w| w == json),
            "payload bytes are not a byte-exact slice of the fixture"
        );
    }

    #[test]
    fn from_response_address_with_geographies() {
        let body = fixture("geo_address_all.json");
        let s = UpstreamSingle::from_response(&body).unwrap();
        assert_eq!(s.status, Status::Match);
        assert_eq!(s.views, Views::JSON | Views::GEO);
        assert_eq!(s.payload.benchmark_name, "Public_AR_Current");
        assert_eq!(s.payload.vintage_name, "Current_Current");
        let json = s.payload.json.unwrap();
        assert!(json.starts_with(b"[{"));
        assert_passthrough(&body, &json);
    }

    #[test]
    fn from_response_locations_only_and_nomatch() {
        let s = UpstreamSingle::from_response(&fixture("loc_oneline.json")).unwrap();
        assert_eq!((s.status, s.views), (Status::Match, Views::JSON));
        assert_eq!(s.payload.vintage_name, "");

        let s = UpstreamSingle::from_response(&fixture("loc_nomatch.json")).unwrap();
        assert_eq!((s.status, s.views), (Status::NoMatch, Views::JSON));
        assert_eq!(s.payload.json.unwrap().as_slice(), b"[]");
    }

    #[test]
    fn from_response_coordinates() {
        let body = fixture("geo_coords_all.json");
        let s = UpstreamSingle::from_response(&body).unwrap();
        assert_eq!((s.status, s.views), (Status::Match, Views::GEO));
        let json = s.payload.json.unwrap();
        assert!(json.starts_with(b"{\"2020 Census"));
        assert_passthrough(&body, &json);
    }

    #[test]
    fn from_response_rejects_errors() {
        assert!(UpstreamSingle::from_response(&fixture("err_nobenchmark.json")).is_err());
    }
}
