//! Runtime config overrides: dotted path -> JSON value (applied as `Source::Override`).

use redb::ReadableTable;

use super::{Store, StoreError, tables::CONFIG_OVERRIDES};

impl Store {
    pub fn overrides(&self) -> Result<Vec<(String, String)>, StoreError> {
        self.read(|txn| {
            let t = txn.open_table(CONFIG_OVERRIDES)?;
            t.iter()?
                .map(|item| {
                    let (k, v) = item?;
                    Ok((k.value().to_owned(), v.value().to_owned()))
                })
                .collect()
        })
    }

    pub async fn set_override(&self, path: String, json: String) -> Result<(), StoreError> {
        self.write(Box::new(move |ctx| {
            ctx.config_overrides()?
                .insert(path.as_str(), json.as_str())?;
            Ok(())
        }))
        .await
    }

    /// Set several overrides in one transaction.
    pub async fn set_overrides(&self, items: Vec<(String, String)>) -> Result<(), StoreError> {
        self.write(Box::new(move |ctx| {
            let t = ctx.config_overrides()?;
            for (path, json) in &items {
                t.insert(path.as_str(), json.as_str())?;
            }
            Ok(())
        }))
        .await
    }

    pub async fn delete_override(&self, path: String) -> Result<(), StoreError> {
        self.write(Box::new(move |ctx| {
            ctx.config_overrides()?.remove(path.as_str())?;
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
    async fn set_list_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&opts(dir.path())).unwrap();
        store
            .set_override("throttle.single_rps".into(), "2".into())
            .await
            .unwrap();
        store
            .set_override("ttl.tie".into(), "\"1h\"".into())
            .await
            .unwrap();
        store
            .set_override("throttle.single_rps".into(), "3".into())
            .await
            .unwrap();
        assert_eq!(
            store.overrides().unwrap(),
            vec![
                ("throttle.single_rps".to_owned(), "3".to_owned()),
                ("ttl.tie".to_owned(), "\"1h\"".to_owned())
            ]
        );
        store.delete_override("ttl.tie".into()).await.unwrap();
        assert_eq!(store.overrides().unwrap().len(), 1);
    }
}
