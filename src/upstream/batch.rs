//! Batch uploads (`addressbatch`, `coordinatesbatch`).

use reqwest::multipart::{Form, Part};

use super::{Kind, Upstream, UpstreamError, UpstreamResponse, throttle::Priority};

/// Which batch endpoint to call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchEndpoint {
    /// `{locations|geographies}/addressbatch`.
    Address { geographies: bool },
    /// `geographies/coordinatesbatch`.
    Coordinates,
}

impl BatchEndpoint {
    fn path_and_field(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Address { geographies: false } => {
                ("locations/addressbatch", "addressFile", "addresses.csv")
            }
            Self::Address { geographies: true } => {
                ("geographies/addressbatch", "addressFile", "addresses.csv")
            }
            Self::Coordinates => (
                "geographies/coordinatesbatch",
                "coordinatesFile",
                "coordinates.csv",
            ),
        }
    }
}

impl Upstream {
    /// Upload one chunk. The multipart form is rebuilt for every retry attempt.
    pub async fn fetch_batch(
        &self,
        prio: Priority,
        endpoint: BatchEndpoint,
        benchmark_id: u32,
        vintage_id: Option<u32>,
        csv: Vec<u8>,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let (path, field, file_name) = endpoint.path_and_field();
        let url = format!("{}/{path}", self.base_url());
        self.execute(Kind::Batch, prio, |c| {
            let part = Part::bytes(csv.clone())
                .file_name(file_name)
                .mime_str("text/csv")
                .expect("static MIME type is valid");
            let mut form = Form::new()
                .part(field, part)
                .text("benchmark", benchmark_id.to_string());
            if let Some(v) = vintage_id {
                form = form.text("vintage", v.to_string());
            }
            c.post(&url).multipart(form)
        })
        .await
    }
}
