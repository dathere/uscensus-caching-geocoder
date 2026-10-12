//! `layers=` filtering over stored `geographies` objects. Layer arrays are kept as
//! `RawValue`s, so filtered output is byte-identical to upstream for every kept layer.

use indexmap::IndexMap;
use serde_json::value::RawValue;

use crate::catalog::Catalog;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerFilter {
    All,
    Names(Vec<String>),
}

impl LayerFilter {
    /// `layers` omitted -> the vintage's default set (all if unknown); `all` -> all;
    /// otherwise comma-separated TIGERweb layer ids or names.
    pub fn resolve(param: Option<&str>, vintage_name: &str, catalog: &Catalog) -> Self {
        let Some(p) = param.map(str::trim).filter(|p| !p.is_empty()) else {
            return catalog
                .default_layers(vintage_name)
                .map_or(Self::All, |d| Self::Names(d.to_vec()));
        };
        if p.eq_ignore_ascii_case("all") {
            return Self::All;
        }
        let names = p
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .filter_map(|t| match t.parse::<u32>() {
                Ok(id) => catalog.layer_name(vintage_name, id).map(str::to_owned),
                Err(_) => Some(t.to_owned()),
            })
            .collect();
        Self::Names(names)
    }

    fn keeps(&self, layer: &str) -> bool {
        match self {
            Self::All => true,
            Self::Names(n) => n.iter().any(|x| x == layer),
        }
    }
}

/// Filter a raw `geographies` object, preserving upstream order and bytes.
pub fn filter_geographies(
    raw: &RawValue,
    filter: &LayerFilter,
) -> serde_json::Result<Box<RawValue>> {
    if *filter == LayerFilter::All {
        return Ok(raw.to_owned());
    }
    let mut layers: IndexMap<String, &RawValue> = serde_json::from_str(raw.get())?;
    layers.retain(|name, _| filter.keeps(name));
    serde_json::value::to_raw_value(&layers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo_fixture() -> String {
        let v: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/live/geo_coords_all.json"
        ))
        .unwrap();
        serde_json::to_string(&v["result"]["geographies"]).unwrap()
    }

    #[test]
    fn resolve_variants() {
        let c = Catalog::snapshot();
        assert_eq!(
            LayerFilter::resolve(Some("all"), "Current_Current", &c),
            LayerFilter::All
        );
        assert_eq!(
            LayerFilter::resolve(Some("8, 12,States,99999"), "Current_Current", &c),
            LayerFilter::Names(vec![
                "Census Tracts".into(),
                "2020 Census Blocks".into(),
                "States".into()
            ])
        );
        let LayerFilter::Names(d) = LayerFilter::resolve(None, "Current_Current", &c) else {
            panic!()
        };
        assert!(d.contains(&"States".to_string()));
        assert_eq!(
            LayerFilter::resolve(None, "Unknown_Vintage", &c),
            LayerFilter::All
        );
    }

    #[test]
    fn filtering_keeps_bytes_and_order() {
        let text = geo_fixture();
        let raw: Box<RawValue> = serde_json::from_str(&text).unwrap();
        let f = LayerFilter::Names(vec!["Census Tracts".into(), "States".into()]);
        let out = filter_geographies(&raw, &f).unwrap();
        let kept: IndexMap<String, &RawValue> = serde_json::from_str(out.get()).unwrap();
        assert_eq!(kept.keys().collect::<Vec<_>>(), ["States", "Census Tracts"]);
        for v in kept.values() {
            assert!(text.contains(v.get()));
        }
        assert_eq!(
            filter_geographies(&raw, &LayerFilter::All).unwrap().get(),
            raw.get()
        );
    }
}
