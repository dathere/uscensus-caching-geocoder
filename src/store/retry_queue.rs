//! Queue of NoMatch entries to re-check: `(due unix secs, key) -> attempts`.

use std::ops::Bound;

use redb::ReadableTableMetadata;

use super::{Store, StoreError, tables::RETRY_QUEUE};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryItem {
    pub due: u64,
    pub key: String,
    pub attempts: u8,
}

impl Store {
    pub async fn retry_enqueue(
        &self,
        key: String,
        due: u64,
        attempts: u8,
    ) -> Result<(), StoreError> {
        self.write(Box::new(move |ctx| {
            ctx.retry_queue()?.insert((due, key.as_str()), attempts)?;
            Ok(())
        }))
        .await
    }

    /// Items due at or before `now`, oldest first.
    pub fn retry_due(&self, now: u64, limit: usize) -> Result<Vec<RetryItem>, StoreError> {
        self.read(|txn| {
            let t = txn.open_table(RETRY_QUEUE)?;
            let end = Bound::Excluded((now.saturating_add(1), ""));
            let mut out = Vec::new();
            for item in t.range::<(u64, &str)>((Bound::Unbounded, end))?.take(limit) {
                let (k, v) = item?;
                let (due, key) = k.value();
                out.push(RetryItem {
                    due,
                    key: key.to_owned(),
                    attempts: v.value(),
                });
            }
            Ok(out)
        })
    }

    pub fn retry_len(&self) -> Result<u64, StoreError> {
        self.read(|txn| Ok(txn.open_table(RETRY_QUEUE)?.len()?))
    }

    pub async fn retry_remove(&self, due: u64, key: String) -> Result<(), StoreError> {
        self.write(Box::new(move |ctx| {
            ctx.retry_queue()?.remove((due, key.as_str()))?;
            Ok(())
        }))
        .await
    }

    /// Move an item to a new due time with an updated attempt count.
    pub async fn retry_reschedule(
        &self,
        item: RetryItem,
        new_due: u64,
        attempts: u8,
    ) -> Result<(), StoreError> {
        self.write(Box::new(move |ctx| {
            let t = ctx.retry_queue()?;
            t.remove((item.due, item.key.as_str()))?;
            t.insert((new_due, item.key.as_str()), attempts)?;
            Ok(())
        }))
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::opts;
    use super::*;

    #[tokio::test]
    async fn due_ordering_and_reschedule() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&opts(dir.path())).unwrap();
        store.retry_enqueue("b".into(), 200, 0).await.unwrap();
        store.retry_enqueue("a".into(), 100, 1).await.unwrap();
        store.retry_enqueue("c".into(), 300, 0).await.unwrap();

        let due = store.retry_due(200, 10).unwrap();
        assert_eq!(
            due.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(due[0].attempts, 1);
        assert_eq!(store.retry_due(99, 10).unwrap(), vec![]);
        assert_eq!(store.retry_due(1000, 1).unwrap().len(), 1);

        store
            .retry_reschedule(due[0].clone(), 400, 2)
            .await
            .unwrap();
        store.retry_remove(200, "b".into()).await.unwrap();
        assert_eq!(store.retry_len().unwrap(), 2);
        let all = store.retry_due(u64::MAX, 10).unwrap();
        assert_eq!(
            all,
            vec![
                RetryItem {
                    due: 300,
                    key: "c".into(),
                    attempts: 0
                },
                RetryItem {
                    due: 400,
                    key: "a".into(),
                    attempts: 2
                },
            ]
        );
    }
}
