//! Catalog fetchers: `/benchmarks`, `/vintages`, TIGERweb MapServer layer lists.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{Kind, Upstream, UpstreamError, UpstreamResponse, throttle::Priority};
use crate::catalog::tigerweb_suffix;

#[derive(Deserialize)]
struct MapServer {
    layers: Vec<MapLayer>,
}

#[derive(Deserialize)]
struct MapLayer {
    id: u32,
    name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogFetchError {
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    #[error("unexpected TIGERweb response: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Layer ids -> names, skipping label layers (they are not geocoder layers).
pub fn parse_mapserver_layers(body: &[u8]) -> Result<BTreeMap<u32, String>, serde_json::Error> {
    let doc: MapServer = serde_json::from_slice(body)?;
    Ok(doc
        .layers
        .into_iter()
        .filter(|l| !l.name.contains("Labels"))
        .map(|l| (l.id, l.name))
        .collect())
}

impl Upstream {
    pub async fn fetch_benchmarks(
        &self,
        prio: Priority,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let url = format!("{}/benchmarks", self.base_url());
        self.execute(Kind::Single, prio, |c| c.get(&url)).await
    }

    pub async fn fetch_vintages(
        &self,
        prio: Priority,
        benchmark: &str,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let url = format!("{}/vintages", self.base_url());
        self.execute(Kind::Single, prio, |c| {
            c.get(&url).query(&[("benchmark", benchmark)])
        })
        .await
    }

    /// Layers of the TIGERweb MapServer backing `vintage_name` (`tigerWMS_<suffix>`).
    pub async fn fetch_tigerweb_layers(
        &self,
        prio: Priority,
        vintage_name: &str,
    ) -> Result<BTreeMap<u32, String>, CatalogFetchError> {
        let url = format!(
            "{}/tigerWMS_{}/MapServer",
            self.tigerweb_url(),
            tigerweb_suffix(vintage_name)
        );
        let resp = self
            .execute(Kind::Single, prio, |c| c.get(&url).query(&[("f", "json")]))
            .await?;
        Ok(parse_mapserver_layers(&resp.body)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_live_mapserver_snapshot() {
        let layers = parse_mapserver_layers(include_bytes!(
            "../../tests/fixtures/live/tigerweb_current.json"
        ))
        .unwrap();
        assert_eq!(layers.len(), 40);
        assert_eq!(layers[&8], "Census Tracts");
        assert!(layers.values().all(|n| !n.contains("Labels")));
    }
}
