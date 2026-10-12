//! Two-tier upstream throttle (PLAN.md §7).
//!
//! * `Sync` (a client is waiting): register as a waiter, queue on the semaphore, then
//!   wait for a rate-limiter token.
//! * `Background` (jobs): proceed only when no `Sync` caller is waiting and both a
//!   permit and a token are free right now; otherwise poll every `background_poll_ms`.
//!
//! Batch uploads additionally keep `batch_min_interval_secs` between starts. A circuit
//! breaker opens after `breaker_threshold` consecutive transient failures.

use std::{
    num::NonZeroU32,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwap;
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::ThrottleConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Sync,
    Background,
}

/// Longest breaker open period after repeated re-opens.
pub const BREAKER_MAX_OPEN: Duration = Duration::from_secs(15 * 60);

#[derive(Debug)]
struct Breaker {
    threshold: u32,
    base_open: Duration,
    consecutive_failures: u32,
    open_for: Duration,
    open_until: Option<Instant>,
}

impl Breaker {
    fn new(threshold: u32, base_open: Duration) -> Self {
        Self {
            threshold: threshold.max(1),
            base_open,
            consecutive_failures: 0,
            open_for: base_open,
            open_until: None,
        }
    }

    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.open_until.filter(|u| *u > now).map(|u| u - now)
    }
}

/// Snapshot of the breaker for health/admin endpoints.
#[derive(Debug, Clone, Serialize)]
pub struct BreakerState {
    pub open: bool,
    pub consecutive_failures: u32,
    /// Seconds until the breaker half-opens (0 when closed).
    pub retry_after_secs: u64,
    /// Unix seconds when it half-opens, if open.
    pub open_until: Option<u64>,
}

/// Returned by `Sync` acquires while the breaker is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("upstream circuit breaker is open; retry in {}s", .retry_after.as_secs().max(1))]
pub struct BreakerOpen {
    pub retry_after: Duration,
}

pub struct Throttle {
    limiter: ArcSwap<DefaultDirectRateLimiter>,
    single_sem: Arc<Semaphore>,
    batch_sem: Arc<Semaphore>,
    single_size: AtomicUsize,
    batch_size: AtomicUsize,
    sync_waiters: AtomicUsize,
    batch_sync_waiters: AtomicUsize,
    batch_min_interval: Mutex<Duration>,
    background_poll: Mutex<Duration>,
    last_batch_start: tokio::sync::Mutex<Option<Instant>>,
    breaker: Mutex<Breaker>,
}

fn quota(cfg: &ThrottleConfig) -> Quota {
    let rps = NonZeroU32::new(cfg.single_rps).unwrap_or(NonZeroU32::MIN);
    let burst = NonZeroU32::new(cfg.single_burst).unwrap_or(NonZeroU32::MIN);
    Quota::per_second(rps).allow_burst(burst)
}

/// Decrements a waiter counter even if the acquiring future is cancelled.
struct WaiterGuard<'a>(&'a AtomicUsize);

impl<'a> WaiterGuard<'a> {
    fn new(c: &'a AtomicUsize) -> Self {
        c.fetch_add(1, Ordering::SeqCst);
        Self(c)
    }
}

impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Throttle {
    pub fn new(cfg: &ThrottleConfig) -> Self {
        Self {
            limiter: ArcSwap::from_pointee(RateLimiter::direct(quota(cfg))),
            single_sem: Arc::new(Semaphore::new(cfg.single_concurrency)),
            batch_sem: Arc::new(Semaphore::new(cfg.batch_concurrency)),
            single_size: AtomicUsize::new(cfg.single_concurrency),
            batch_size: AtomicUsize::new(cfg.batch_concurrency),
            sync_waiters: AtomicUsize::new(0),
            batch_sync_waiters: AtomicUsize::new(0),
            batch_min_interval: Mutex::new(Duration::from_secs(cfg.batch_min_interval_secs)),
            background_poll: Mutex::new(Duration::from_millis(cfg.background_poll_ms.max(1))),
            last_batch_start: tokio::sync::Mutex::new(None),
            breaker: Mutex::new(Breaker::new(
                cfg.breaker_threshold,
                Duration::from_secs(cfg.breaker_open_secs),
            )),
        }
    }

    /// Apply new limits at runtime (rebuilds the limiter, resizes semaphores).
    pub fn reconfigure(&self, cfg: &ThrottleConfig) {
        self.limiter
            .store(Arc::new(RateLimiter::direct(quota(cfg))));
        resize(&self.single_sem, &self.single_size, cfg.single_concurrency);
        resize(&self.batch_sem, &self.batch_size, cfg.batch_concurrency);
        *lock(&self.batch_min_interval) = Duration::from_secs(cfg.batch_min_interval_secs);
        *lock(&self.background_poll) = Duration::from_millis(cfg.background_poll_ms.max(1));
        let mut b = lock(&self.breaker);
        b.threshold = cfg.breaker_threshold.max(1);
        b.base_open = Duration::from_secs(cfg.breaker_open_secs);
    }

    /// Permit for one single-record request; hold it for the request's duration.
    pub async fn acquire_single(
        &self,
        prio: Priority,
    ) -> Result<OwnedSemaphorePermit, BreakerOpen> {
        match prio {
            Priority::Sync => {
                self.check_breaker()?;
                let _waiting = WaiterGuard::new(&self.sync_waiters);
                let permit = Arc::clone(&self.single_sem)
                    .acquire_owned()
                    .await
                    .expect("throttle semaphores are never closed");
                self.limiter.load_full().until_ready().await;
                Ok(permit)
            }
            Priority::Background => loop {
                if let Some(wait) = self.breaker_remaining() {
                    tokio::time::sleep(wait).await;
                    continue;
                }
                if self.sync_waiters.load(Ordering::SeqCst) == 0
                    && let Ok(permit) = Arc::clone(&self.single_sem).try_acquire_owned()
                {
                    if self.limiter.load().check().is_ok() {
                        return Ok(permit);
                    }
                    drop(permit);
                }
                tokio::time::sleep(self.poll()).await;
            },
        }
    }

    /// Permit for one batch upload; enforces the minimum gap between upload starts.
    pub async fn acquire_batch(&self, prio: Priority) -> Result<OwnedSemaphorePermit, BreakerOpen> {
        match prio {
            Priority::Sync => {
                self.check_breaker()?;
                let _waiting = WaiterGuard::new(&self.batch_sync_waiters);
                let permit = Arc::clone(&self.batch_sem)
                    .acquire_owned()
                    .await
                    .expect("throttle semaphores are never closed");
                let mut last = self.last_batch_start.lock().await;
                let gap = *lock(&self.batch_min_interval);
                if let Some(prev) = *last {
                    tokio::time::sleep_until((prev + gap).into()).await;
                }
                *last = Some(Instant::now());
                Ok(permit)
            }
            Priority::Background => loop {
                if let Some(wait) = self.breaker_remaining() {
                    tokio::time::sleep(wait).await;
                    continue;
                }
                if self.batch_sync_waiters.load(Ordering::SeqCst) == 0
                    && let Ok(permit) = Arc::clone(&self.batch_sem).try_acquire_owned()
                {
                    let gap = *lock(&self.batch_min_interval);
                    if let Ok(mut last) = self.last_batch_start.try_lock()
                        && last.is_none_or(|prev| prev.elapsed() >= gap)
                    {
                        *last = Some(Instant::now());
                        return Ok(permit);
                    }
                    drop(permit);
                }
                tokio::time::sleep(self.poll()).await;
            },
        }
    }

    fn poll(&self) -> Duration {
        *lock(&self.background_poll)
    }

    fn breaker_remaining(&self) -> Option<Duration> {
        lock(&self.breaker).remaining(Instant::now())
    }

    fn check_breaker(&self) -> Result<(), BreakerOpen> {
        match self.breaker_remaining() {
            Some(retry_after) => Err(BreakerOpen { retry_after }),
            None => Ok(()),
        }
    }

    /// Any upstream answer that is not a transient failure closes the breaker.
    pub fn record_success(&self) {
        let mut b = lock(&self.breaker);
        b.consecutive_failures = 0;
        b.open_until = None;
        b.open_for = b.base_open;
    }

    /// A transient failure; returns `true` if this opened (or re-opened) the breaker.
    pub fn record_failure(&self) -> bool {
        let mut b = lock(&self.breaker);
        b.consecutive_failures = b.consecutive_failures.saturating_add(1);
        if b.consecutive_failures < b.threshold || b.remaining(Instant::now()).is_some() {
            return false;
        }
        let was_opened_before = b.open_until.is_some();
        if was_opened_before {
            b.open_for = (b.open_for * 2).min(BREAKER_MAX_OPEN);
        }
        b.open_until = Some(Instant::now() + b.open_for);
        true
    }

    /// Manually close the breaker (admin action).
    pub fn reset_breaker(&self) {
        self.record_success();
    }

    pub fn breaker_state(&self) -> BreakerState {
        let b = lock(&self.breaker);
        let remaining = b.remaining(Instant::now());
        BreakerState {
            open: remaining.is_some(),
            consecutive_failures: b.consecutive_failures,
            retry_after_secs: remaining.map_or(0, |d| d.as_secs().max(1)),
            open_until: remaining.map(|d| {
                (SystemTime::now() + d)
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |t| t.as_secs())
            }),
        }
    }

    /// Callers currently queued at `Sync` priority (single, batch).
    pub fn sync_waiters(&self) -> (usize, usize) {
        (
            self.sync_waiters.load(Ordering::SeqCst),
            self.batch_sync_waiters.load(Ordering::SeqCst),
        )
    }

    /// Permits currently free (single, batch).
    pub fn available_permits(&self) -> (usize, usize) {
        (
            self.single_sem.available_permits(),
            self.batch_sem.available_permits(),
        )
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn resize(sem: &Semaphore, size: &AtomicUsize, target: usize) {
    let target = target.max(1);
    let cur = size.load(Ordering::SeqCst);
    if target > cur {
        sem.add_permits(target - cur);
        size.store(target, Ordering::SeqCst);
    } else if target < cur {
        // Only idle permits can be forgotten; busy ones shrink the pool as they return
        // on the next reconfigure.
        let forgotten = sem.forget_permits(cur - target);
        size.store(cur - forgotten, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg() -> ThrottleConfig {
        let mut c = Config::default().throttle;
        c.single_rps = 1000;
        c.single_burst = 1000;
        c.single_concurrency = 1;
        c.background_poll_ms = 5;
        c.batch_min_interval_secs = 0;
        c
    }

    #[tokio::test]
    async fn background_yields_to_sync_waiters() {
        let t = Arc::new(Throttle::new(&cfg()));
        let held = t.acquire_single(Priority::Sync).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let (t1, tx1) = (Arc::clone(&t), tx.clone());
        let sync_task = tokio::spawn(async move {
            let p = t1.acquire_single(Priority::Sync).await.unwrap();
            tx1.send("sync").unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(p);
        });
        while t.sync_waiters().0 == 0 {
            tokio::task::yield_now().await;
        }
        let (t2, tx2) = (Arc::clone(&t), tx.clone());
        let bg_task = tokio::spawn(async move {
            let _p = t2.acquire_single(Priority::Background).await.unwrap();
            tx2.send("background").unwrap();
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            rx.try_recv().is_err(),
            "nobody may proceed while the permit is held"
        );
        drop(held);
        assert_eq!(rx.recv().await, Some("sync"));
        assert!(
            rx.try_recv().is_err(),
            "background must wait for the sync caller to finish"
        );
        assert_eq!(rx.recv().await, Some("background"));
        sync_task.await.unwrap();
        bg_task.await.unwrap();
    }

    #[tokio::test]
    async fn background_does_not_jump_a_queued_sync_caller() {
        // Even when a permit is momentarily free, a registered sync waiter wins.
        let t = Throttle::new(&cfg());
        let _waiting = WaiterGuard::new(&t.sync_waiters);
        let bg = tokio::time::timeout(
            Duration::from_millis(40),
            t.acquire_single(Priority::Background),
        )
        .await;
        assert!(bg.is_err());
    }

    #[test]
    fn breaker_opens_at_threshold_doubles_and_resets() {
        let mut c = cfg();
        c.breaker_threshold = 3;
        c.breaker_open_secs = 60;
        let t = Throttle::new(&c);
        assert!(!t.record_failure());
        assert!(!t.record_failure());
        assert!(t.record_failure());
        let s = t.breaker_state();
        assert!(s.open && s.retry_after_secs > 55);
        assert!(t.check_breaker().is_err());

        // Simulate the open period elapsing, then fail again: re-open with double time.
        lock(&t.breaker).open_until = Some(Instant::now());
        assert!(t.record_failure());
        assert_eq!(lock(&t.breaker).open_for, Duration::from_secs(120));

        t.record_success();
        let s = t.breaker_state();
        assert!(!s.open);
        assert_eq!(s.consecutive_failures, 0);
        assert_eq!(lock(&t.breaker).open_for, Duration::from_secs(60));
    }

    #[test]
    fn breaker_open_period_caps() {
        let mut c = cfg();
        c.breaker_threshold = 1;
        c.breaker_open_secs = 600;
        let t = Throttle::new(&c);
        for _ in 0..5 {
            lock(&t.breaker).open_until = Some(Instant::now());
            t.record_failure();
        }
        assert_eq!(lock(&t.breaker).open_for, BREAKER_MAX_OPEN);
    }

    #[tokio::test]
    async fn batch_min_interval_spaces_uploads() {
        let mut c = cfg();
        c.batch_min_interval_secs = 1;
        let t = Throttle::new(&c);
        let start = Instant::now();
        drop(t.acquire_batch(Priority::Sync).await.unwrap());
        drop(t.acquire_batch(Priority::Sync).await.unwrap());
        assert!(start.elapsed() >= Duration::from_millis(990));
    }

    #[tokio::test]
    async fn reconfigure_resizes_concurrency() {
        let t = Throttle::new(&cfg());
        assert_eq!(t.available_permits().0, 1);
        let mut c = cfg();
        c.single_concurrency = 3;
        t.reconfigure(&c);
        assert_eq!(t.available_permits().0, 3);
        c.single_concurrency = 2;
        t.reconfigure(&c);
        assert_eq!(t.available_permits().0, 2);
    }
}
