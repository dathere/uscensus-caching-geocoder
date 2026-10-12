//! Retry classification and delays (PLAN.md §7).

use std::time::{Duration, SystemTime};

use reqwest::{StatusCode, header::HeaderMap};

/// Transient upstream statuses worth retrying. 400/404 and other 4xx never are.
pub fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

/// Parse `Retry-After` as delta-seconds or an HTTP-date relative to `now`.
pub fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let v = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(v).ok()?;
    Some(at.duration_since(now).unwrap_or_default())
}

/// Delay before retry number `attempt` (0-based): `Retry-After` when given, else
/// `base * 2^attempt` plus up to `base` of jitter; always capped at `cap`.
pub fn delay(
    attempt: u32,
    base: Duration,
    cap: Duration,
    retry_after: Option<Duration>,
) -> Duration {
    if let Some(ra) = retry_after {
        return ra.min(cap);
    }
    let exp = base.saturating_mul(1u32 << attempt.min(16));
    let jitter = Duration::from_nanos(fastrand::u64(
        0..=u64::try_from(base.as_nanos()).unwrap_or(u64::MAX),
    ));
    exp.saturating_add(jitter).min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderValue, RETRY_AFTER};

    fn headers(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_str(v).unwrap());
        h
    }

    #[test]
    fn retryable_statuses() {
        for s in [408, 429, 500, 502, 503, 504] {
            assert!(is_retryable(StatusCode::from_u16(s).unwrap()), "{s}");
        }
        for s in [200, 400, 401, 404, 422] {
            assert!(!is_retryable(StatusCode::from_u16(s).unwrap()), "{s}");
        }
    }

    #[test]
    fn retry_after_forms() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_760_000_000);
        assert_eq!(
            retry_after(&headers("7"), now),
            Some(Duration::from_secs(7))
        );
        let date = httpdate::fmt_http_date(now + Duration::from_secs(30));
        assert_eq!(
            retry_after(&headers(&date), now),
            Some(Duration::from_secs(30))
        );
        let past = httpdate::fmt_http_date(now - Duration::from_secs(30));
        assert_eq!(retry_after(&headers(&past), now), Some(Duration::ZERO));
        assert_eq!(retry_after(&headers("soon"), now), None);
        assert_eq!(retry_after(&HeaderMap::new(), now), None);
    }

    #[test]
    fn delays_grow_and_cap() {
        let base = Duration::from_millis(100);
        let cap = Duration::from_secs(60);
        for attempt in 0..4 {
            let d = delay(attempt, base, cap, None);
            let lo = base * (1 << attempt);
            assert!(d >= lo && d <= lo + base, "attempt {attempt}: {d:?}");
        }
        assert_eq!(delay(30, base, cap, None), cap);
        assert_eq!(
            delay(0, base, cap, Some(Duration::from_secs(5))),
            Duration::from_secs(5)
        );
        assert_eq!(delay(0, base, cap, Some(Duration::from_secs(600))), cap);
    }
}
