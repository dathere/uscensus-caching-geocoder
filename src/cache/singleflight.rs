//! Coalesce concurrent identical upstream fetches.
//!
//! The first caller for a key spawns the work as a task, so a client disconnecting does
//! not cancel a fetch others are waiting on. Every caller (including the first) awaits
//! the task's result on a `watch` channel.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, PoisonError},
};

use tokio::sync::watch;

pub struct Singleflight<T> {
    inflight: Mutex<HashMap<String, watch::Receiver<Option<T>>>>,
}

impl<T> Default for Singleflight<T> {
    fn default() -> Self {
        Self {
            inflight: Mutex::new(HashMap::new()),
        }
    }
}

/// Removes the in-flight entry even if the task panics.
struct Remove<T> {
    sf: Arc<Singleflight<T>>,
    key: String,
}

impl<T> Drop for Remove<T> {
    fn drop(&mut self) {
        self.sf
            .inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
    }
}

impl<T: Clone + Send + Sync + 'static> Singleflight<T> {
    /// Run `make()` once per key at a time; concurrent callers share its result.
    /// Returns `None` only if the task panicked.
    pub async fn run<F, Fut>(self: &Arc<Self>, key: String, make: F) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        let mut rx = {
            let mut map = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(rx) = map.get(&key) {
                rx.clone()
            } else {
                let (tx, rx) = watch::channel(None);
                map.insert(key.clone(), rx.clone());
                let fut = make();
                let guard = Remove {
                    sf: Arc::clone(self),
                    key,
                };
                tokio::spawn(async move {
                    let _guard = guard;
                    let v = fut.await;
                    let _ = tx.send(Some(v));
                });
                rx
            }
        };
        let v = rx.wait_for(Option::is_some).await.ok()?;
        v.clone()
    }

    pub fn in_flight(&self) -> usize {
        self.inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use super::*;

    #[tokio::test]
    async fn concurrent_callers_share_one_run() {
        let sf = Arc::new(Singleflight::<u32>::default());
        let runs = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..20 {
            let (sf, runs) = (Arc::clone(&sf), Arc::clone(&runs));
            tasks.push(tokio::spawn(async move {
                sf.run("k".into(), || async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    7
                })
                .await
            }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap(), Some(7));
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(sf.in_flight(), 0);
    }

    #[tokio::test]
    async fn caller_cancellation_does_not_cancel_the_work() {
        let sf = Arc::new(Singleflight::<u32>::default());
        let done = Arc::new(AtomicUsize::new(0));
        let d = Arc::clone(&done);
        let first = {
            let sf = Arc::clone(&sf);
            tokio::spawn(async move {
                sf.run("k".into(), || async move {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    d.fetch_add(1, Ordering::SeqCst);
                    1
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(5)).await;
        first.abort();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(done.load(Ordering::SeqCst), 1);
        assert_eq!(sf.in_flight(), 0);
    }

    #[tokio::test]
    async fn panicking_work_is_cleaned_up() {
        let sf = Arc::new(Singleflight::<u32>::default());
        let r = sf.run("k".into(), || async { panic!("boom") }).await;
        assert_eq!(r, None);
        tokio::task::yield_now().await;
        assert_eq!(sf.in_flight(), 0);
        assert_eq!(sf.run("k".into(), || async { 2 }).await, Some(2));
    }
}
