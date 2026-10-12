//! Single-record lookups.

use super::{Kind, Upstream, UpstreamError, UpstreamResponse, throttle::Priority};
use crate::cache::key::KeyInput;

/// What to ask upstream for one record. With `vintage` set the request is
/// `geographies/...&layers=all` (the cache always stores every layer); without it,
/// `locations/...` (only used when `fetch_geographies_always` is off).
#[derive(Debug, Clone, Copy)]
pub struct SingleQuery<'a> {
    pub input: &'a KeyInput,
    pub benchmark_id: u32,
    pub vintage_id: Option<u32>,
}

impl SingleQuery<'_> {
    /// Path below the base URL and query parameters.
    pub fn path_and_params(&self) -> (String, Vec<(&'static str, String)>) {
        let mut params: Vec<(&'static str, String)> = Vec::new();
        let mut push = |k: &'static str, v: &str| {
            if !v.trim().is_empty() {
                params.push((k, v.to_owned()));
            }
        };
        let search = match self.input {
            KeyInput::Oneline { address } => {
                push("address", address);
                "onelineaddress"
            }
            KeyInput::Address {
                street,
                city,
                state,
                zip,
            } => {
                push("street", street);
                push("city", city);
                push("state", state);
                push("zip", zip);
                "address"
            }
            KeyInput::AddressPR {
                street,
                urb,
                city,
                municipio,
                state,
                zip,
            } => {
                push("street", street);
                push("urb", urb);
                push("city", city);
                push("municipio", municipio);
                push("state", state);
                push("zip", zip);
                "addressPR"
            }
            KeyInput::Coordinates { x, y } => {
                push("x", &x.to_string());
                push("y", &y.to_string());
                "coordinates"
            }
        };
        let returntype =
            if self.vintage_id.is_some() || matches!(self.input, KeyInput::Coordinates { .. }) {
                "geographies"
            } else {
                "locations"
            };
        params.push(("benchmark", self.benchmark_id.to_string()));
        if let Some(v) = self.vintage_id {
            params.push(("vintage", v.to_string()));
            params.push(("layers", "all".to_owned()));
        }
        params.push(("format", "json".to_owned()));
        (format!("{returntype}/{search}"), params)
    }
}

impl Upstream {
    pub async fn fetch_single(
        &self,
        prio: Priority,
        q: SingleQuery<'_>,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let (path, params) = q.path_and_params();
        let url = format!("{}/{path}", self.base_url());
        self.execute(Kind::Single, prio, |c| c.get(&url).query(&params))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geographies_address_query() {
        let input = KeyInput::Address {
            street: "4600 Silver Hill Rd".into(),
            city: "Washington".into(),
            state: "DC".into(),
            zip: String::new(),
        };
        let (path, params) = SingleQuery {
            input: &input,
            benchmark_id: 4,
            vintage_id: Some(4),
        }
        .path_and_params();
        assert_eq!(path, "geographies/address");
        assert_eq!(
            params,
            vec![
                ("street", "4600 Silver Hill Rd".to_owned()),
                ("city", "Washington".to_owned()),
                ("state", "DC".to_owned()),
                ("benchmark", "4".to_owned()),
                ("vintage", "4".to_owned()),
                ("layers", "all".to_owned()),
                ("format", "json".to_owned()),
            ]
        );
    }

    #[test]
    fn locations_and_coordinates() {
        let one = KeyInput::Oneline {
            address: "x".into(),
        };
        let (path, params) = SingleQuery {
            input: &one,
            benchmark_id: 4,
            vintage_id: None,
        }
        .path_and_params();
        assert_eq!(path, "locations/onelineaddress");
        assert!(!params.iter().any(|(k, _)| *k == "layers"));

        let c = KeyInput::Coordinates {
            x: -76.92744,
            y: 38.84601,
        };
        let (path, params) = SingleQuery {
            input: &c,
            benchmark_id: 4,
            vintage_id: Some(4),
        }
        .path_and_params();
        assert_eq!(path, "geographies/coordinates");
        assert_eq!(params[0], ("x", "-76.92744".to_owned()));
    }
}
