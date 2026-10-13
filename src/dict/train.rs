//! `ucgeocoder train-dict`: train the zstd dictionary from captured upstream responses.
//!
//! Each sample is a raw single-record response (see `scripts/capture.py samples`). It is
//! converted to exactly what the store compresses, the msgpack-encoded
//! [`EntryPayload`](crate::cache::envelope::EntryPayload), so the dictionary matches
//! the real data. Every 10th sample is held out to report an honest ratio.

use std::path::Path;

use anyhow::{Context, bail};

use crate::cache::{codec, envelope::UpstreamSingle};

/// zstd's default dictionary size.
pub const DEFAULT_MAX_SIZE: usize = 112_640;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainReport {
    pub samples: usize,
    pub skipped: usize,
    pub dict_id: u32,
    pub dict_bytes: usize,
    pub holdout: usize,
    pub holdout_raw_bytes: usize,
    pub holdout_zstd_bytes: usize,
    pub holdout_dict_bytes: usize,
}

impl std::fmt::Display for TrainReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ratio = |n: usize| self.holdout_raw_bytes as f64 / n.max(1) as f64;
        write!(
            f,
            "trained dict id {} ({} bytes) from {} samples ({} skipped)\n\
             holdout of {}: raw {} B, zstd {} B ({:.1}x), zstd+dict {} B ({:.1}x)",
            self.dict_id,
            self.dict_bytes,
            self.samples,
            self.skipped,
            self.holdout,
            self.holdout_raw_bytes,
            self.holdout_zstd_bytes,
            ratio(self.holdout_zstd_bytes),
            self.holdout_dict_bytes,
            ratio(self.holdout_dict_bytes),
        )
    }
}

/// Convert every `*.json` file in `dir` to payload bytes. Unparseable responses
/// (errors, HTML) are counted as skipped.
pub fn load_samples(dir: &Path) -> anyhow::Result<(Vec<Vec<u8>>, usize)> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    let mut samples = Vec::with_capacity(paths.len());
    let mut skipped = 0;
    for p in paths {
        let body = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
        match UpstreamSingle::from_response(&body) {
            Ok(s) => samples.push(rmp_serde::to_vec_named(&s.payload)?),
            Err(_) => skipped += 1,
        }
    }
    Ok((samples, skipped))
}

/// Train a dictionary and measure it on held-out samples.
pub fn train(
    samples: &[Vec<u8>],
    skipped: usize,
    max_size: usize,
) -> anyhow::Result<(Vec<u8>, TrainReport)> {
    if samples.len() < 20 {
        bail!("need at least 20 usable samples, have {}", samples.len());
    }
    let (holdout, training): (Vec<_>, Vec<_>) =
        samples.iter().enumerate().partition(|(i, _)| i % 10 == 0);
    let training: Vec<&[u8]> = training.into_iter().map(|(_, s)| s.as_slice()).collect();
    let dict = zstd::dict::from_samples(&training, max_size).context("zstd dictionary training")?;
    let d = codec::Dictionary::new(&dict)?;
    let dict_id = d.id();
    let with_dict = codec::Codec::new(Some(d), vec![]);
    let plain = codec::Codec::default();

    let mut report = TrainReport {
        samples: samples.len(),
        skipped,
        dict_id,
        dict_bytes: dict.len(),
        holdout: holdout.len(),
        holdout_raw_bytes: 0,
        holdout_zstd_bytes: 0,
        holdout_dict_bytes: 0,
    };
    for (_, s) in holdout {
        report.holdout_raw_bytes += s.len();
        report.holdout_zstd_bytes += plain.compress(s)?.2.len();
        report.holdout_dict_bytes += with_dict.compress(s)?.2.len();
    }
    Ok((dict, report))
}

/// Load samples from `samples_dir`, train, and write the dictionary to `out`.
///
/// If `out` already holds a dictionary, it is first moved to `census.prev.dict` next to
/// it, which the binary embeds as a decode-only dictionary: without it every entry
/// compressed before the retrain would become an unknown-dictionary miss.
pub fn run(samples_dir: &Path, out: &Path, max_size: usize) -> anyhow::Result<TrainReport> {
    let (samples, skipped) = load_samples(samples_dir)?;
    let (dict, report) = train(&samples, skipped, max_size)?;
    if let Ok(old) = std::fs::read(out)
        && codec::dict_id(&old).is_some_and(|id| id != report.dict_id)
    {
        let prev = out.with_file_name("census.prev.dict");
        std::fs::write(&prev, &old).with_context(|| format!("writing {}", prev.display()))?;
    }
    std::fs::write(out, &dict).with_context(|| format!("writing {}", out.display()))?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trains_from_fixture_like_samples() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/live/geo_coords_all.json"
        ))
        .unwrap();
        for i in 0..60 {
            let body = base
                .replace("Maryland", &format!("State{i}"))
                .replace("24033", &format!("{:05}", i * 7));
            std::fs::write(dir.path().join(format!("{i:04}.json")), body).unwrap();
        }
        std::fs::write(dir.path().join("bad.json"), "<html>").unwrap();
        let out = dir.path().join("census.dict");
        let report = run(dir.path(), &out, 16 * 1024).unwrap();
        assert_eq!((report.samples, report.skipped, report.holdout), (60, 1, 6));
        assert_eq!(
            codec::dict_id(&std::fs::read(&out).unwrap()),
            Some(report.dict_id)
        );
        assert!(report.holdout_dict_bytes < report.holdout_zstd_bytes);
    }

    #[test]
    fn retrain_keeps_previous_dictionary() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/live/geo_coords_all.json"
        ))
        .unwrap();
        let write = |n: usize, tag: &str| {
            for i in 0..n {
                let body = base.replace("Maryland", &format!("{tag}{i}"));
                std::fs::write(dir.path().join(format!("{tag}{i:04}.json")), body).unwrap();
            }
        };
        let out = dir.path().join("census.dict");
        let prev = dir.path().join("census.prev.dict");

        write(40, "a");
        let first = run(dir.path(), &out, 8 * 1024).unwrap();
        assert!(!prev.exists(), "nothing to preserve on the first run");

        write(40, "b");
        let second = run(dir.path(), &out, 8 * 1024).unwrap();
        assert_ne!(first.dict_id, second.dict_id);
        assert_eq!(
            codec::dict_id(&std::fs::read(&prev).unwrap()),
            Some(first.dict_id)
        );
        assert_eq!(
            codec::dict_id(&std::fs::read(&out).unwrap()),
            Some(second.dict_id)
        );
    }

    #[test]
    fn too_few_samples() {
        assert!(train(&[b"x".to_vec()], 0, 1024).is_err());
    }
}
