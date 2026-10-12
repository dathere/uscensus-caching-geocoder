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
    let mut dirty = false;
    let mut last_durable = Instant::now();
    let idle_wait = Duration::from_secs(3600);
    loop {
        let wait = if dirty {
            cfg.durable_every.saturating_sub(last_durable.elapsed())
        } else {
            idle_wait
        };
        let first = match rx.recv_timeout(wait) {
            Ok(m) => m,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if dirty {
                    match commit(db, &[], true) {
                        Ok(()) => {
                            dirty = false;
                            last_durable = Instant::now();
                        }
                        Err(e) => error!(error = %e, "periodic durable commit failed"),
                    }
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Msg::Shutdown,
        };

        let mut ops: Vec<(Op, Option<Ack>)> = Vec::new();
        let mut flushes: Vec<Ack> = Vec::new();
        let mut shutdown = false;
        let deadline = Instant::now() + cfg.flush;
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

        let durable =
            shutdown || !flushes.is_empty() || last_durable.elapsed() >= cfg.durable_every;
        if durable || !ops.is_empty() {
            let ok = apply(db, ops, durable);
            if durable && ok {
                dirty = false;
                last_durable = Instant::now();
            } else {
                dirty = true;
            }
            for ack in flushes {
                let _ = ack.send(if ok {
                    Ok(())
                } else {
                    Err(StoreError::Write("durable commit failed".into()))
                });
            }
        }
        if shutdown {
            return;
        }
    }
}

/// Commit `ops` together; on failure retry each alone. Returns whether every commit
/// (including the durability barrier when `ops` is empty) succeeded.
fn apply(db: &RwLock<Database>, ops: Vec<(Op, Option<Ack>)>, durable: bool) -> bool {
    let batch: Vec<&Op> = ops.iter().map(|(op, _)| op).collect();
    match commit(db, &batch, durable) {
        Ok(()) => {
            for (_, ack) in ops {
                if let Some(ack) = ack {
                    let _ = ack.send(Ok(()));
                }
            }
            true
        }
        Err(e) if ops.len() > 1 => {
            warn!(error = %e, ops = ops.len(), "batch commit failed; retrying ops individually");
            let mut all_ok = true;
            for (op, ack) in ops {
                let r = commit(db, &[&op], durable);
                all_ok &= r.is_ok();
                if let Some(ack) = ack {
                    let _ = ack.send(r);
                }
            }
            all_ok
        }
        Err(e) => {
            error!(error = %e, "commit failed");
            if let Some((_, Some(ack))) = ops.into_iter().next() {
                let _ = ack.send(Err(e));
            }
            false
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
