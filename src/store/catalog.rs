//! Cached catalog payloads: `benchmarks`, `vintages:{b}`, `layers:{v}` -> `(raw JSON, fetched_at)`.

use serde_bytes::ByteBuf;

use super::{Store, StoreError, tables::CATALOG};

impl Store {
    /// Raw JSON and fetch time (unix secs) for a catalog item.
    pub fn catalog_get(&self, name: &str) -> Result<Option<(Vec<u8>, u64)>, StoreError> {
        let raw = self.read(|txn| {
            Ok(txn
                .open_table(CATALOG)?
                .get(name)?
                .map(|g| g.value().to_vec()))
        })?;
        raw.map(|b| {
            let (json, at): (ByteBuf, u64) =
                rmp_serde::from_slice(&b).map_err(|e| StoreError::Encoding(e.to_string()))?;
            Ok((json.into_vec(), at))
        })
        .transpose()
    }

    pub async fn catalog_put(
        &self,
        name: String,
        raw_json: Vec<u8>,
        fetched_at: u64,
    ) -> Result<(), StoreError> {
        let value = rmp_serde::to_vec(&(serde_bytes::Bytes::new(&raw_json), fetched_at))
            .map_err(|e| StoreError::Encoding(e.to_string()))?;
        self.write(Box::new(move |ctx| {
            ctx.catalog()?.insert(name.as_str(), value.as_slice())?;
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
    async fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&opts(dir.path())).unwrap();
        assert_eq!(store.catalog_get("benchmarks").unwrap(), None);
        store
            .catalog_put(
                "benchmarks".into(),
                br#"{"benchmarks":[]}"#.to_vec(),
                1_760_000_000,
            )
            .await
            .unwrap();
        assert_eq!(
            store.catalog_get("benchmarks").unwrap(),
            Some((br#"{"benchmarks":[]}"#.to_vec(), 1_760_000_000))
        );
    }
}
