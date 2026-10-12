//! The single writer thread.
//!
//! All mutations are boxed ops sent over a channel. The thread batches them into one
//! write transaction per `flush` window (or `max_batch` ops), committing with
//! `Durability::None`, and makes everything durable with an `Immediate` commit at
//! least every `durable_every`, on [`Msg::Flush`], and at shutdown. Each op is acked
//! after the commit that contains it. Ops must be re-runnable (`Fn`): if a batch fails,
//! its ops are retried one per transaction so one bad op cannot sink the others.
//!
//! The channel is unbounded; batch and job chunk sizes (≤ 10k rows) bound it in practice.

use std::{
    sync::{Arc, Mutex, PoisonError, RwLock, mpsc},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use redb::{Database, Durability, Table, WriteTransaction};
use tokio::sync::oneshot;
use tracing::{error, warn};

use super::{StoreError, tables};

/// A re-runnable mutation applied inside a write transaction.
pub type Op = Box<dyn Fn(&mut WriteCtx<'_>) -> Result<(), StoreError> + Send>;
type Ack = oneshot::Sender<Result<(), StoreError>>;

enum Msg {
    Op(Op, Option<Ack>),
    Flush(Ack),
    Shutdown,
}

#[derive(Debug, Clone, Copy)]
pub struct WriterConfig {
    /// Batching window after the first queued op.
    pub flush: Duration,
    /// Maximum ops per transaction.
    pub max_batch: usize,
    /// Upper bound on how long a non-durable commit stays non-durable.
    pub durable_every: Duration,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            flush: Duration::from_millis(50),
            max_batch: 500,
            durable_every: Duration::from_secs(30),
        }
    }
}

macro_rules! lazy_tables {
    ($($field:ident: $def:path => $k:ty, $v:ty;)*) => {
        /// Tables opened lazily, at most once per transaction.
        pub struct WriteCtx<'t> {
            txn: &'t WriteTransaction,
            $($field: Option<Table<'t, $k, $v>>,)*
        }

        impl<'t> WriteCtx<'t> {
            fn new(txn: &'t WriteTransaction) -> Self {
                Self { txn, $($field: None,)* }
            }

            $(
                pub fn $field(&mut self) -> Result<&mut Table<'t, $k, $v>, StoreError> {
                    if self.$field.is_none() {
                        self.$field = Some(self.txn.open_table($def)?);
                    }
                    Ok(self.$field.as_mut().expect("just opened"))
                }
            )*
        }
    };
}

lazy_tables! {
    entries: tables::ENTRIES => &'static str, &'static [u8];
    jobs: tables::JOBS => &'static str, &'static [u8];
    job_rows: tables::JOB_ROWS => (&'static str, u32), &'static [u8];
    job_chunks: tables::JOB_CHUNKS => (&'static str, u32), &'static [u8];
    stats: tables::STATS => &'static str, u64;
    catalog: tables::CATALOG => &'static str, &'static [u8];
    retry_queue: tables::RETRY_QUEUE => (u64, &'static str), u8;
    config_overrides: tables::CONFIG_OVERRIDES => &'static str, &'static str;
    meta: tables::META => &'static str, &'static str;
}

/// Owns the writer thread; dropping it drains the queue, commits durably, and joins.
pub(crate) struct WriterHandle {
    tx: mpsc::Sender<Msg>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl WriterHandle {
    pub(crate) fn spawn(db: Arc<RwLock<Database>>, cfg: WriterConfig) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("ucg-writer".into())
            .spawn(move || run(&db, &rx, cfg))?;
        Ok(Self {
            tx,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Queue `op` and wait until the transaction containing it has committed.
    pub(crate) async fn write(&self, op: Op) -> Result<(), StoreError> {
        let (ack, done) = oneshot::channel();
        self.tx
            .send(Msg::Op(op, Some(ack)))
            .map_err(|_| StoreError::Closed)?;
        done.await.map_err(|_| StoreError::Closed)?
    }

    /// Commit everything queued so far with `Durability::Immediate`.
    pub(crate) async fn flush(&self) -> Result<(), StoreError> {
        let (ack, done) = oneshot::channel();
        self.tx
            .send(Msg::Flush(ack))
            .map_err(|_| StoreError::Closed)?;
        done.await.map_err(|_| StoreError::Closed)?
    }
}

impl Drop for WriterHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        let handle = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(h) = handle
            && h.join().is_err()
        {
            error!("store writer thread panicked");
        }
    }
}

fn run(db: &RwLock<Database>, rx: &mpsc::Receiver<Msg>, cfg: WriterConfig) {
    let mut sched = Schedule::new(cfg.durable_every, Instant::now());
    // Set after an error redb treats as terminal (it refuses writes after a failed
    // I/O); from then on every request fails fast instead of retrying forever.
    let mut broken = false;
    loop {
        let first = match rx.recv_timeout(sched.idle_wait(Instant::now())) {
            Ok(m) => m,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !broken && sched.due(Instant::now()) {
                    let outcome = apply(db, Vec::new(), true);
                    broken |= outcome == Outcome::Broken;
                    sched.on_commit(true, outcome == Outcome::Ok, Instant::now());
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Msg::Shutdown,
        };

        let mut ops: Vec<(Op, Option<Ack>)> = Vec::new();
        let mut flushes: Vec<Ack> = Vec::new();
        let mut shutdown = false;
        let deadline = sched.batch_deadline(Instant::now(), cfg.flush);
        let mut next = Some(first);
        loop {
            match next.take() {
                Some(Msg::Op(op, ack)) => ops.push((op, ack)),
                Some(Msg::Flush(ack)) => flushes.push(ack),
                Some(Msg::Shutdown) => shutdown = true,
                None => {}
            }
            if shutdown || !flushes.is_empty() || ops.len() >= cfg.max_batch.max(1) {
                break;
            }
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(m) => next = Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => shutdown = true,
            }
        }
        if shutdown {
            // Anything queued behind the shutdown still gets written.
            while let Ok(m) = rx.try_recv() {
                match m {
                    Msg::Op(op, ack) => ops.push((op, ack)),
                    Msg::Flush(ack) => flushes.push(ack),
                    Msg::Shutdown => {}
                }
            }
        }

        let outcome = if broken {
            for (_, ack) in ops {
                if let Some(ack) = ack {
                    let _ = ack.send(Err(broken_error()));
                }
            }
            Outcome::Broken
        } else {
            let durable = shutdown || !flushes.is_empty() || sched.due(Instant::now());
            if durable || !ops.is_empty() {
                let had_ops = !ops.is_empty();
                let outcome = apply(db, ops, durable);
                if durable || had_ops {
                    sched.on_commit(durable, outcome == Outcome::Ok, Instant::now());
                }
                outcome
            } else {
                Outcome::Ok
            }
        };
        if outcome == Outcome::Broken && !broken {
            broken = true;
            error!(
                "store writer stopped: redb refuses further writes after an I/O error; restart required"
            );
        }
        for ack in flushes {
            let _ = ack.send(match outcome {
                Outcome::Ok => Ok(()),
                Outcome::Failed => Err(StoreError::Write("durable commit failed".into())),
                Outcome::Broken => Err(broken_error()),
            });
        }
        if shutdown {
            return;
        }
    }
}

fn broken_error() -> StoreError {
    StoreError::Write("store is in a failed state after an I/O error; restart required".into())
}

/// Errors after which redb will not accept further writes.
fn is_terminal(e: &StoreError) -> bool {
    matches!(e, StoreError::Redb(redb::Error::PreviousIo))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Ok,
    Failed,
    Broken,
}

/// When the next durable commit is due. Pure, so the timing rules are unit-testable.
#[derive(Debug, Clone, Copy)]
struct Schedule {
    durable_every: Duration,
    /// A non-durable commit happened since the last durable one.
    dirty: bool,
    last_durable: Instant,
    /// Consecutive failed durable commits; drives the retry backoff.
    failures: u32,
}

impl Schedule {
    const IDLE_WAIT: Duration = Duration::from_secs(3600);
    const MAX_BACKOFF: Duration = Duration::from_secs(60);

    fn new(durable_every: Duration, now: Instant) -> Self {
        Self {
            durable_every,
            dirty: false,
            last_durable: now,
            failures: 0,
        }
    }

    fn backoff(&self) -> Duration {
        match self.failures {
            0 => Duration::ZERO,
            n => Duration::from_secs(1u64 << (n - 1).min(6)).min(Self::MAX_BACKOFF),
        }
    }

    /// The instant a durable commit becomes due, if one is owed.
    fn durable_deadline(&self) -> Option<Instant> {
        self.dirty
            .then(|| self.last_durable + self.durable_every + self.backoff())
    }

    fn due(&self, now: Instant) -> bool {
        self.durable_deadline().is_some_and(|d| now >= d)
    }

    /// How long to block waiting for the first message of a batch.
    fn idle_wait(&self, now: Instant) -> Duration {
        self.durable_deadline()
            .map_or(Self::IDLE_WAIT, |d| d.saturating_duration_since(now))
    }

    /// End of the batching window: never later than a pending durability deadline.
    fn batch_deadline(&self, now: Instant, flush: Duration) -> Instant {
        let window = now + flush;
        self.durable_deadline()
            .map_or(window, |d| window.min(d.max(now)))
    }

    fn on_commit(&mut self, durable: bool, ok: bool, now: Instant) {
        match (durable, ok) {
            (true, true) => {
                self.dirty = false;
                self.last_durable = now;
                self.failures = 0;
            }
            (false, true) => self.dirty = true,
            (true, false) => {
                // Keep owing a durable commit, but back off before the next try.
                self.dirty = true;
                self.last_durable = now;
                self.failures = self.failures.saturating_add(1);
            }
            (false, false) => {}
        }
    }
}

fn commit(db: &RwLock<Database>, ops: &[&Op], durable: bool) -> Result<(), StoreError> {
    let guard = db.read().unwrap_or_else(PoisonError::into_inner);
    let mut txn = guard.begin_write()?;
    txn.set_durability(if durable {
        Durability::Immediate
    } else {
        Durability::None
    })?;
    {
        let mut ctx = WriteCtx::new(&txn);
        for op in ops {
            op(&mut ctx)?;
        }
    }
    txn.commit()?;
    drop(guard);
    Ok(())
}

/// Commit `ops` together; on a non-terminal failure retry each alone. With no ops this
/// is a bare durability barrier.
fn apply(db: &RwLock<Database>, ops: Vec<(Op, Option<Ack>)>, durable: bool) -> Outcome {
    let batch: Vec<&Op> = ops.iter().map(|(op, _)| op).collect();
    match commit(db, &batch, durable) {
        Ok(()) => {
            for (_, ack) in ops {
                if let Some(ack) = ack {
                    let _ = ack.send(Ok(()));
                }
            }
            Outcome::Ok
        }
        Err(e) if is_terminal(&e) => {
            error!(error = %e, "commit failed with a terminal storage error");
            for (_, ack) in ops {
                if let Some(ack) = ack {
                    let _ = ack.send(Err(broken_error()));
                }
            }
            Outcome::Broken
        }
        Err(e) if ops.len() > 1 => {
            warn!(error = %e, ops = ops.len(), "batch commit failed; retrying ops individually");
            let mut outcome = Outcome::Ok;
            for (op, ack) in ops {
                let r = if outcome == Outcome::Broken {
                    Err(broken_error())
                } else {
                    commit(db, &[&op], durable)
                };
                match &r {
                    Ok(()) => {}
                    Err(e) if is_terminal(e) => outcome = Outcome::Broken,
                    Err(_) if outcome == Outcome::Ok => outcome = Outcome::Failed,
                    Err(_) => {}
                }
                if let Some(ack) = ack {
                    let _ = ack.send(r);
                }
            }
            outcome
        }
        Err(e) => {
            error!(error = %e, "commit failed");
            if let Some((_, Some(ack))) = ops.into_iter().next() {
                let _ = ack.send(Err(e));
            }
            Outcome::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn clean_schedule_waits_idle() {
        let t0 = Instant::now();
        let s = Schedule::new(30 * S, t0);
        assert!(!s.due(t0 + 3600 * S));
        assert_eq!(s.idle_wait(t0), Schedule::IDLE_WAIT);
        assert_eq!(s.batch_deadline(t0, 5 * S), t0 + 5 * S);
    }

    #[test]
    fn durable_commit_due_after_interval() {
        let t0 = Instant::now();
        let mut s = Schedule::new(30 * S, t0);
        s.on_commit(false, true, t0 + S);
        assert!(!s.due(t0 + 29 * S));
        assert!(s.due(t0 + 30 * S));
        assert_eq!(s.idle_wait(t0 + 10 * S), 20 * S);
        s.on_commit(true, true, t0 + 30 * S);
        assert!(!s.due(t0 + 1000 * S));
    }

    #[test]
    fn batch_window_never_outlasts_durability_deadline() {
        // A long flush window must not postpone a pending durable commit.
        let t0 = Instant::now();
        let mut s = Schedule::new(S, t0);
        s.on_commit(false, true, t0);
        let near_deadline = t0 + Duration::from_millis(900);
        assert_eq!(s.batch_deadline(near_deadline, 10 * S), t0 + S);
        // Already overdue: the window closes immediately.
        assert_eq!(s.batch_deadline(t0 + 2 * S, 10 * S), t0 + 2 * S);
    }

    #[test]
    fn failed_durable_commit_backs_off_instead_of_spinning() {
        let t0 = Instant::now();
        let mut s = Schedule::new(S, t0);
        s.on_commit(false, true, t0);
        let mut now = t0 + S;
        let mut waits = Vec::new();
        for _ in 0..10 {
            assert!(s.due(now));
            s.on_commit(true, false, now);
            let wait = s.idle_wait(now);
            assert!(wait >= S, "retry must not be immediate: {wait:?}");
            waits.push(wait);
            now += wait;
        }
        assert!(waits.windows(2).all(|w| w[1] >= w[0]), "{waits:?}");
        assert_eq!(*waits.last().unwrap(), S + Schedule::MAX_BACKOFF);
        s.on_commit(true, true, now);
        assert_eq!((s.failures, s.dirty), (0, false));
    }

    #[test]
    fn previous_io_is_terminal() {
        assert!(is_terminal(&StoreError::Redb(redb::Error::PreviousIo)));
        assert!(!is_terminal(&StoreError::Write("x".into())));
    }
}
