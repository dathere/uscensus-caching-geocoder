//! zstd compression with a trained dictionary embedded in the binary.
//!
//! Each envelope records the `dict_id` it was compressed with, so the dictionary can be
//! rotated: the decoder keeps the current dictionary plus any previous ones, and an
//! entry referencing an unknown id is reported as [`CodecError::UnknownDict`] (callers
//! treat that as a miss and delete the entry).

use std::sync::{Arc, OnceLock};

use zstd::dict::{DecoderDictionary, EncoderDictionary};

/// The dictionary shipped in `resources/census.dict`. An empty file means "no
/// dictionary" (used only to bootstrap `train-dict`).
pub static EMBEDDED_DICT: &[u8] = include_bytes!("../../resources/census.dict");

/// Compression level. Writes are rare relative to reads, so favour ratio.
pub const LEVEL: i32 = 9;

/// Refuse to decompress frames claiming more than this (defends against corrupt headers).
const MAX_DECOMPRESSED: u64 = 64 * 1024 * 1024;

const DICT_MAGIC: [u8; 4] = 0xEC30_A437_u32.to_le_bytes();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CodecKind {
    Raw = 0,
    ZstdDict = 1,
    Zstd = 2,
}

impl CodecKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Raw),
            1 => Some(Self::ZstdDict),
            2 => Some(Self::Zstd),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("entry was compressed with unknown dictionary id {0}")]
    UnknownDict(u32),
    #[error("dictionary is not a zstd dictionary (bad magic)")]
    BadDict,
    #[error("zstd frame content size is missing or too large")]
    FrameSize,
    #[error("zstd: {0}")]
    Zstd(#[from] std::io::Error),
}

pub struct Dictionary {
    id: u32,
    enc: EncoderDictionary<'static>,
    dec: DecoderDictionary<'static>,
}

impl std::fmt::Debug for Dictionary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dictionary")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Dictionary {
    /// Parse a zstd dictionary (as produced by `ZDICT_trainFromBuffer`).
    pub fn new(raw: &[u8]) -> Result<Self, CodecError> {
        let id = dict_id(raw).ok_or(CodecError::BadDict)?;
        Ok(Self {
            id,
            enc: EncoderDictionary::copy(raw, LEVEL),
            dec: DecoderDictionary::copy(raw),
        })
    }

    pub fn id(&self) -> u32 {
        self.id
    }
}

/// Read the dictionary id from a zstd dictionary header; `None` if not a dictionary.
pub fn dict_id(raw: &[u8]) -> Option<u32> {
    if raw.len() < 8 || raw[..4] != DICT_MAGIC {
        return None;
    }
    Some(u32::from_le_bytes(raw[4..8].try_into().ok()?))
}

#[derive(Debug, Default)]
pub struct Codec {
    current: Option<Arc<Dictionary>>,
    previous: Vec<Arc<Dictionary>>,
}

impl Codec {
    pub fn new(current: Option<Dictionary>, previous: Vec<Dictionary>) -> Self {
        Self {
            current: current.map(Arc::new),
            previous: previous.into_iter().map(Arc::new).collect(),
        }
    }

    /// Codec using the dictionary embedded at build time (shared, built once).
    pub fn embedded() -> &'static Codec {
        static CODEC: OnceLock<Codec> = OnceLock::new();
        CODEC.get_or_init(|| {
            let dict = if EMBEDDED_DICT.is_empty() {
                None
            } else {
                Some(
                    Dictionary::new(EMBEDDED_DICT)
                        .expect("resources/census.dict is not a valid zstd dictionary"),
                )
            };
            Codec::new(dict, Vec::new())
        })
    }

    /// Id written into new envelopes (0 when there is no dictionary).
    pub fn current_dict_id(&self) -> u32 {
        self.current.as_ref().map_or(0, |d| d.id)
    }

    /// Compress `raw`, falling back to [`CodecKind::Raw`] when zstd does not help.
    /// Returns the codec used, the dictionary id (0 unless `ZstdDict`), and the bytes.
    pub fn compress(&self, raw: &[u8]) -> Result<(CodecKind, u32, Vec<u8>), CodecError> {
        let (kind, id, out) = match &self.current {
            Some(d) => {
                let mut c = zstd::bulk::Compressor::with_prepared_dictionary(&d.enc)?;
                (CodecKind::ZstdDict, d.id, c.compress(raw)?)
            }
            None => (CodecKind::Zstd, 0, zstd::bulk::compress(raw, LEVEL)?),
        };
        if out.len() >= raw.len() {
            Ok((CodecKind::Raw, 0, raw.to_vec()))
        } else {
            Ok((kind, id, out))
        }
    }

    pub fn decompress(
        &self,
        kind: CodecKind,
        dict_id: u32,
        data: &[u8],
    ) -> Result<Vec<u8>, CodecError> {
        match kind {
            CodecKind::Raw => Ok(data.to_vec()),
            CodecKind::Zstd => {
                Ok(zstd::bulk::Decompressor::new()?.decompress(data, frame_size(data)?)?)
            }
            CodecKind::ZstdDict => {
                let dict = self.find(dict_id).ok_or(CodecError::UnknownDict(dict_id))?;
                let mut d = zstd::bulk::Decompressor::with_prepared_dictionary(&dict.dec)?;
                Ok(d.decompress(data, frame_size(data)?)?)
            }
        }
    }

    fn find(&self, id: u32) -> Option<&Arc<Dictionary>> {
        self.current
            .iter()
            .chain(self.previous.iter())
            .find(|d| d.id == id)
    }
}

fn frame_size(data: &[u8]) -> Result<usize, CodecError> {
    match zstd::zstd_safe::get_frame_content_size(data) {
        Ok(Some(n)) if n <= MAX_DECOMPRESSED => {
            Ok(usize::try_from(n).map_err(|_| CodecError::FrameSize)?)
        }
        _ => Err(CodecError::FrameSize),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small dictionary trained on synthetic Census-like payloads, for tests that must
    /// not depend on the embedded one.
    pub(crate) fn test_dict() -> Vec<u8> {
        let samples: Vec<Vec<u8>> = (0..400)
            .map(|i| {
                format!(
                    r#"{{"States":[{{"STATENS":"0171{i:04}","GEOID":"{i:02}","CENTLAT":"+38.94{i}","NAME":"State {i}"}}],"Counties":[{{"GEOID":"{i:05}","BASENAME":"County {i}","LSADC":"06"}}]}}"#
                )
                .into_bytes()
            })
            .collect();
        zstd::dict::from_samples(&samples, 4096).unwrap()
    }

    #[test]
    fn dict_round_trip() {
        let raw = test_dict();
        let d = Dictionary::new(&raw).unwrap();
        let id = d.id();
        assert_ne!(id, 0);
        let codec = Codec::new(Some(d), vec![]);
        let payload = br#"{"States":[{"STATENS":"01714934","GEOID":"24","CENTLAT":"+38.9463607","NAME":"Maryland"}],"Counties":[{"GEOID":"24033","BASENAME":"Prince George's","LSADC":"06"}]}"#;
        let (kind, got_id, out) = codec.compress(payload).unwrap();
        assert_eq!((kind, got_id), (CodecKind::ZstdDict, id));
        assert!(out.len() < payload.len());
        assert_eq!(codec.decompress(kind, got_id, &out).unwrap(), payload);
    }

    #[test]
    fn incompressible_falls_back_to_raw() {
        let codec = Codec::default();
        let (kind, id, out) = codec.compress(b"ab").unwrap();
        assert_eq!((kind, id, out.as_slice()), (CodecKind::Raw, 0, &b"ab"[..]));
    }

    #[test]
    fn plain_zstd_without_dictionary() {
        let codec = Codec::default();
        let payload = "abcdefgh".repeat(100);
        let (kind, id, out) = codec.compress(payload.as_bytes()).unwrap();
        assert_eq!((kind, id), (CodecKind::Zstd, 0));
        assert_eq!(
            codec.decompress(kind, id, &out).unwrap(),
            payload.as_bytes()
        );
    }

    #[test]
    fn unknown_dict_id_is_reported() {
        let raw = test_dict();
        let writer = Codec::new(Some(Dictionary::new(&raw).unwrap()), vec![]);
        let payload = "GEOID".repeat(50);
        let (kind, id, out) = writer.compress(payload.as_bytes()).unwrap();
        let reader = Codec::default();
        assert!(
            matches!(reader.decompress(kind, id, &out), Err(CodecError::UnknownDict(i)) if i == id)
        );
    }

    #[test]
    fn previous_dictionaries_still_decode() {
        let raw = test_dict();
        let old = Codec::new(Some(Dictionary::new(&raw).unwrap()), vec![]);
        let payload = "GEOID BASENAME".repeat(20);
        let (kind, id, out) = old.compress(payload.as_bytes()).unwrap();
        let rotated = Codec::new(None, vec![Dictionary::new(&raw).unwrap()]);
        assert_eq!(
            rotated.decompress(kind, id, &out).unwrap(),
            payload.as_bytes()
        );
    }

    #[test]
    fn rejects_non_dictionary() {
        assert!(matches!(
            Dictionary::new(b"not a dict"),
            Err(CodecError::BadDict)
        ));
        assert_eq!(dict_id(&[]), None);
    }

    #[test]
    fn embedded_dictionary_is_valid() {
        assert!(
            !EMBEDDED_DICT.is_empty(),
            "resources/census.dict must be a trained dictionary"
        );
        assert_eq!(
            Codec::embedded().current_dict_id(),
            dict_id(EMBEDDED_DICT).unwrap()
        );
    }
}
