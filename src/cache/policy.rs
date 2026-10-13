//! TTL policy (PLAN.md §5.5).

use std::time::{Duration, SystemTime};

use super::envelope::{EntrySource, Status};
use crate::config::TtlConfig;

/// Benchmarks whose data drifts behind a stable id (`Current`, `ACS*`, `LUCA`) versus
/// frozen ones (e.g. `Public_AR_Census2020`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchmarkClass {
    Current,
    Fixed,
}

impl BenchmarkClass {
    pub fn from_name(benchmark_name: &str) -> Self {
        if ["Current", "ACS", "LUCA"]
            .iter()
            .any(|m| benchmark_name.contains(m))
        {
            Self::Current
        } else {
            Self::Fixed
        }
    }
}

/// Time-to-live for an entry; `None` means it never expires.
pub fn ttl(
    cfg: &TtlConfig,
    status: Status,
    source: EntrySource,
    class: BenchmarkClass,
) -> Option<Duration> {
    let d = match (status, class) {
        (Status::Match, BenchmarkClass::Current) => cfg.match_current,
        (Status::Match, BenchmarkClass::Fixed) => cfg.match_fixed,
        (Status::Tie, _) => cfg.tie,
        (Status::NoMatch, _) => match source {
            EntrySource::Single | EntrySource::Import | EntrySource::Confirmed => {
                cfg.nomatch_single
            }
            EntrySource::Batch | EntrySource::Job => cfg.nomatch_batch,
        },
    };
    (!d.is_zero()).then_some(d)
}

/// Batch/job NoMatch results are load-dependent upstream, so they are re-checked later.
pub fn needs_negative_retry(status: Status, source: EntrySource) -> bool {
    status == Status::NoMatch && matches!(source, EntrySource::Batch | EntrySource::Job)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Within TTL: serve as a HIT.
    Fresh,
    /// Expired, but may be served (`X-Cache: STALE`) if the upstream refetch fails.
    Stale,
    /// Expired and not servable: a miss; overwritten on success.
    Expired,
}

pub fn freshness(
    created_at: SystemTime,
    now: SystemTime,
    ttl: Option<Duration>,
    serve_stale_on_error: bool,
) -> Freshness {
    let Some(ttl) = ttl else {
        return Freshness::Fresh;
    };
    // A clock that moved backwards makes the entry look new, not expired.
    let age = now.duration_since(created_at).unwrap_or_default();
    if age < ttl {
        Freshness::Fresh
    } else if serve_stale_on_error {
        Freshness::Stale
    } else {
        Freshness::Expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const DAY: Duration = Duration::from_secs(86_400);

    #[test]
    fn classes() {
        for n in ["Public_AR_Current", "Public_AR_ACS2026", "Public_AR_LUCA"] {
            assert_eq!(BenchmarkClass::from_name(n), BenchmarkClass::Current, "{n}");
        }
        assert_eq!(
            BenchmarkClass::from_name("Public_AR_Census2020"),
            BenchmarkClass::Fixed
        );
    }

    #[test]
    fn ttl_table() {
        let c = Config::default().ttl;
        use BenchmarkClass::*;
        use EntrySource::*;
        use Status::*;
        assert_eq!(ttl(&c, Match, Single, Current), Some(180 * DAY));
        assert_eq!(ttl(&c, Match, Batch, Fixed), None);
        assert_eq!(ttl(&c, Tie, Batch, Fixed), Some(30 * DAY));
        assert_eq!(ttl(&c, NoMatch, Single, Fixed), Some(30 * DAY));
        assert_eq!(ttl(&c, NoMatch, Import, Current), Some(30 * DAY));
        assert_eq!(ttl(&c, NoMatch, Batch, Current), Some(DAY));
        assert_eq!(ttl(&c, NoMatch, Job, Fixed), Some(DAY));
        assert!(needs_negative_retry(NoMatch, Job));
        assert!(!needs_negative_retry(NoMatch, Single));
        assert!(!needs_negative_retry(Match, Batch));
        assert_eq!(ttl(&c, NoMatch, Confirmed, Current), Some(30 * DAY));
        assert!(!needs_negative_retry(NoMatch, Confirmed));
    }

    #[test]
    fn freshness_states() {
        let t0 = SystemTime::UNIX_EPOCH + 1000 * DAY;
        assert_eq!(freshness(t0, t0 + DAY, None, false), Freshness::Fresh);
        assert_eq!(
            freshness(t0, t0 + DAY, Some(2 * DAY), false),
            Freshness::Fresh
        );
        assert_eq!(
            freshness(t0, t0 + 2 * DAY, Some(2 * DAY), true),
            Freshness::Stale
        );
        assert_eq!(
            freshness(t0, t0 + 3 * DAY, Some(2 * DAY), false),
            Freshness::Expired
        );
        assert_eq!(freshness(t0, t0 - DAY, Some(DAY), false), Freshness::Fresh);
    }
}
