//! Benchmarks, vintages and layer names.
//!
//! Built from upstream `/benchmarks` and `/vintages` payloads; the embedded
//! `resources/catalog_snapshot.json` (captured live) is the fallback when upstream has
//! not been reached yet. Benchmark/vintage objects are kept verbatim because they are
//! echoed back in `result.input`.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::cache::policy::BenchmarkClass;

const SNAPSHOT: &str = include_str!("../resources/catalog_snapshot.json");
const DEFAULT_LAYERS: &str = include_str!("../resources/default_layers.json");
const TIGERWEB_LAYERS: &str = include_str!("../resources/tigerweb_layers.json");

#[derive(Debug, Clone, PartialEq)]
pub struct Benchmark {
    pub id: u32,
    pub name: String,
    pub is_default: bool,
    /// The object exactly as upstream sent it (`isDefault`, `benchmarkDescription`, `id`, `benchmarkName`).
    pub raw: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Vintage {
    pub id: u32,
    pub name: String,
    pub is_default: bool,
    pub raw: Map<String, Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("catalog JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("catalog entry missing or invalid `{0}`")]
    Field(&'static str),
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    benchmarks: Vec<Benchmark>,
    vintages: BTreeMap<u32, Vec<Vintage>>,
    default_layers: BTreeMap<String, Vec<String>>,
    tigerweb_layers: BTreeMap<String, BTreeMap<u32, String>>,
}

#[derive(Deserialize)]
struct BenchmarksDoc {
    benchmarks: Vec<Map<String, Value>>,
}

#[derive(Deserialize)]
struct VintagesDoc {
    vintages: Vec<Map<String, Value>>,
}

#[derive(Deserialize)]
struct SnapshotDoc {
    benchmarks: Value,
    vintages: BTreeMap<String, Value>,
}

fn id_of(obj: &Map<String, Value>) -> Result<u32, CatalogError> {
    let id = obj.get("id").ok_or(CatalogError::Field("id"))?;
    match id {
        Value::String(s) => s.parse().map_err(|_| CatalogError::Field("id")),
        Value::Number(n) => n.to_string().parse().map_err(|_| CatalogError::Field("id")),
        _ => Err(CatalogError::Field("id")),
    }
}

fn str_of(obj: &Map<String, Value>, field: &'static str) -> Result<String, CatalogError> {
    obj.get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(CatalogError::Field(field))
}

fn bool_of(obj: &Map<String, Value>) -> bool {
    obj.get("isDefault")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Parse an upstream `/benchmarks` body.
pub fn parse_benchmarks(v: Value) -> Result<Vec<Benchmark>, CatalogError> {
    let doc: BenchmarksDoc = serde_json::from_value(v)?;
    doc.benchmarks
        .into_iter()
        .map(|raw| {
            Ok(Benchmark {
                id: id_of(&raw)?,
                name: str_of(&raw, "benchmarkName")?,
                is_default: bool_of(&raw),
                raw,
            })
        })
        .collect()
}

/// Parse an upstream `/vintages?benchmark=` body.
pub fn parse_vintages(v: Value) -> Result<Vec<Vintage>, CatalogError> {
    let doc: VintagesDoc = serde_json::from_value(v)?;
    doc.vintages
        .into_iter()
        .map(|raw| {
            Ok(Vintage {
                id: id_of(&raw)?,
                name: str_of(&raw, "vintageName")?,
                is_default: bool_of(&raw),
                raw,
            })
        })
        .collect()
}

/// The TIGERweb MapServer suffix for a vintage: `ACS2026_Current` -> `ACS2026`,
/// `Current_Current` -> `Current` (service `tigerWMS_<suffix>`).
pub fn tigerweb_suffix(vintage_name: &str) -> &str {
    vintage_name.split('_').next().unwrap_or(vintage_name)
}

impl Catalog {
    /// The catalog captured into `resources/` at build time.
    pub fn snapshot() -> Self {
        Self::from_snapshot_json(SNAPSHOT, DEFAULT_LAYERS, TIGERWEB_LAYERS)
            .expect("embedded catalog resources are valid")
    }

    pub fn from_snapshot_json(
        snapshot: &str,
        default_layers: &str,
        tigerweb: &str,
    ) -> Result<Self, CatalogError> {
        let doc: SnapshotDoc = serde_json::from_str(snapshot)?;
        let benchmarks = parse_benchmarks(doc.benchmarks)?;
        let mut vintages = BTreeMap::new();
        for (bid, v) in doc.vintages {
            let bid: u32 = bid
                .parse()
                .map_err(|_| CatalogError::Field("vintages key"))?;
            vintages.insert(bid, parse_vintages(v)?);
        }
        let tw: BTreeMap<String, BTreeMap<String, String>> = serde_json::from_str(tigerweb)?;
        let tigerweb_layers = tw
            .into_iter()
            .map(|(suffix, layers)| {
                let layers = layers
                    .into_iter()
                    .map(|(id, name)| {
                        id.parse()
                            .map(|id| (id, name))
                            .map_err(|_| CatalogError::Field("layer id"))
                    })
                    .collect::<Result<_, _>>()?;
                Ok((suffix, layers))
            })
            .collect::<Result<_, CatalogError>>()?;
        Ok(Self {
            benchmarks,
            vintages,
            default_layers: serde_json::from_str(default_layers)?,
            tigerweb_layers,
        })
    }

    /// Replace benchmarks and per-benchmark vintages (e.g. after a live refresh),
    /// keeping layer tables.
    pub fn with_benchmarks(
        mut self,
        benchmarks: Vec<Benchmark>,
        vintages: BTreeMap<u32, Vec<Vintage>>,
    ) -> Self {
        self.benchmarks = benchmarks;
        self.vintages = vintages;
        self
    }

    pub fn benchmarks(&self) -> &[Benchmark] {
        &self.benchmarks
    }

    pub fn vintages(&self, benchmark_id: u32) -> &[Vintage] {
        self.vintages.get(&benchmark_id).map_or(&[], Vec::as_slice)
    }

    pub fn benchmark(&self, id: u32) -> Option<&Benchmark> {
        self.benchmarks.iter().find(|b| b.id == id)
    }

    pub fn vintage(&self, benchmark_id: u32, vintage_id: u32) -> Option<&Vintage> {
        self.vintages(benchmark_id)
            .iter()
            .find(|v| v.id == vintage_id)
    }

    /// Resolve a `benchmark=` parameter: numeric id or name (case-insensitive).
    pub fn resolve_benchmark(&self, alias: &str) -> Option<&Benchmark> {
        let alias = alias.trim();
        match alias.parse::<u32>() {
            Ok(id) => self.benchmark(id),
            Err(_) => self
                .benchmarks
                .iter()
                .find(|b| b.name.eq_ignore_ascii_case(alias)),
        }
    }

    /// Resolve a `vintage=` parameter within a benchmark: numeric id or name.
    pub fn resolve_vintage(&self, benchmark_id: u32, alias: &str) -> Option<&Vintage> {
        let alias = alias.trim();
        match alias.parse::<u32>() {
            Ok(id) => self.vintage(benchmark_id, id),
            Err(_) => self
                .vintages(benchmark_id)
                .iter()
                .find(|v| v.name.eq_ignore_ascii_case(alias)),
        }
    }

    /// The vintage used to key `locations` requests, which carry none, so that
    /// `locations&benchmark=4` and `geographies&benchmark=4&vintage=4` share one entry.
    pub fn default_vintage(&self, benchmark_id: u32) -> Option<&Vintage> {
        let vs = self.vintages(benchmark_id);
        vs.iter().find(|v| v.is_default).or_else(|| vs.first())
    }

    pub fn class(&self, benchmark_id: u32) -> BenchmarkClass {
        self.benchmark(benchmark_id)
            .map_or(BenchmarkClass::Current, |b| {
                BenchmarkClass::from_name(&b.name)
            })
    }

    /// Layer names returned upstream when `layers` is omitted; `None` when unknown
    /// for this vintage (callers then serve every stored layer).
    pub fn default_layers(&self, vintage_name: &str) -> Option<&[String]> {
        self.default_layers.get(vintage_name).map(Vec::as_slice)
    }

    /// Name of TIGERweb layer `layer_id` in the MapServer backing `vintage_name`.
    pub fn layer_name(&self, vintage_name: &str, layer_id: u32) -> Option<&str> {
        self.tigerweb_layers
            .get(tigerweb_suffix(vintage_name))?
            .get(&layer_id)
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_loads() {
        let c = Catalog::snapshot();
        assert!(c.benchmarks().len() >= 3);
        let b = c.benchmark(4).unwrap();
        assert_eq!(b.name, "Public_AR_Current");
        assert_eq!(
            b.raw["benchmarkDescription"],
            "Public Address Ranges - Current Benchmark"
        );
        assert_eq!(c.default_vintage(4).unwrap().name, "Current_Current");
    }

    #[test]
    fn aliases_resolve_to_ids() {
        let c = Catalog::snapshot();
        assert_eq!(c.resolve_benchmark("4").unwrap().id, 4);
        assert_eq!(c.resolve_benchmark("Public_AR_Current").unwrap().id, 4);
        assert_eq!(
            c.resolve_benchmark("public_ar_census2020").unwrap().id,
            2020
        );
        assert!(c.resolve_benchmark("Public_AR_Nope").is_none());
        assert!(c.resolve_benchmark("999").is_none());

        assert_eq!(c.resolve_vintage(4, "4").unwrap().name, "Current_Current");
        assert_eq!(c.resolve_vintage(4, "ACS2026_Current").unwrap().id, 426);
        assert_eq!(c.resolve_vintage(4, "Current_Current").unwrap().id, 4);
        assert!(
            c.resolve_vintage(4, "2020").is_none(),
            "vintage ids are per benchmark"
        );
        assert_eq!(
            c.resolve_vintage(2020, "Census2020_Census2020").unwrap().id,
            2020
        );
    }

    #[test]
    fn raw_objects_match_upstream_input_echo() {
        let c = Catalog::snapshot();
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/live/geo_address_all.json"))
                .unwrap();
        let input = &fixture["result"]["input"];
        assert_eq!(
            Value::Object(c.benchmark(4).unwrap().raw.clone()),
            input["benchmark"]
        );
        assert_eq!(
            Value::Object(c.vintage(4, 4).unwrap().raw.clone()),
            input["vintage"]
        );
    }

    #[test]
    fn classes_by_benchmark() {
        let c = Catalog::snapshot();
        assert_eq!(c.class(4), BenchmarkClass::Current);
        assert_eq!(c.class(8), BenchmarkClass::Current);
        assert_eq!(c.class(2020), BenchmarkClass::Fixed);
    }

    #[test]
    fn layers() {
        let c = Catalog::snapshot();
        let d = c.default_layers("Current_Current").unwrap();
        for l in [
            "States",
            "Census Tracts",
            "2020 Census Blocks",
            "Incorporated Places",
            "Census Designated Places",
        ] {
            assert!(d.iter().any(|x| x == l), "{l}");
        }
        assert!(c.default_layers("LUCA_Current").is_none());
        // Mutually exclusive partners are both present even though a probe point sits
        // in only one of them (ACS2017-2019 use "2010 Census"-prefixed names).
        for (vintage, a, b) in [
            (
                "ACS2018_Current",
                "2010 Census Urbanized Areas",
                "2010 Census Urban Clusters",
            ),
            ("Census2010_Current", "Urbanized Areas", "Urban Clusters"),
            (
                "ACS2026_Current",
                "Incorporated Places",
                "Census Designated Places",
            ),
        ] {
            let d = c.default_layers(vintage).unwrap();
            assert!(
                d.iter().any(|x| x == a) && d.iter().any(|x| x == b),
                "{vintage}"
            );
        }
        assert_eq!(c.layer_name("Current_Current", 8), Some("Census Tracts"));
        assert!(c.layer_name("ACS2026_Current", 8).is_some());
        assert_eq!(tigerweb_suffix("Census2020_Census2020"), "Census2020");
    }

    #[test]
    fn parse_live_payloads() {
        let b = parse_benchmarks(
            serde_json::from_str(include_str!("../tests/fixtures/live/benchmarks.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(b.len(), 4);
        let v = parse_vintages(
            serde_json::from_str(include_str!("../tests/fixtures/live/vintages_4.json")).unwrap(),
        )
        .unwrap();
        assert!(v.iter().any(|v| v.id == 426 && v.name == "ACS2026_Current"));
        let c = Catalog::default().with_benchmarks(b, BTreeMap::from([(4, v)]));
        assert_eq!(c.resolve_vintage(4, "426").unwrap().name, "ACS2026_Current");
    }
}
