//! HTTP error type rendered in the Census error shape:
//! `{"errors":["Benchmark must be specified"],"status":"400"}`.

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{msg}")]
    Unavailable {
        msg: String,
        retry_after_secs: Option<u64>,
    },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// Census-shaped error body.
pub fn census_error_body(status: StatusCode, messages: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "errors": messages,
        "status": status.as_u16().to_string(),
    }))
    .expect("serializing a JSON literal cannot fail")
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        if let Self::Internal(e) = &self {
            tracing::error!(error = %format!("{e:#}"), "internal error");
        }
        let msg = match &self {
            Self::Internal(_) => "Internal server error".to_owned(),
            other => other.to_string(),
        };
        let mut resp = (status, census_error_body(status, &[&msg])).into_response();
        let headers = resp.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if let Self::Unavailable {
            retry_after_secs: Some(secs),
            ..
        } = self
        {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_matches_upstream_bytes() {
        let fixture = include_bytes!("../tests/fixtures/live/err_nobenchmark.json");
        let ours = census_error_body(StatusCode::BAD_REQUEST, &["Benchmark must be specified"]);
        assert_eq!(ours, fixture.trim_ascii_end());
    }

    #[test]
    fn unavailable_sets_retry_after() {
        let resp = AppError::Unavailable {
            msg: "upstream unavailable".into(),
            retry_after_secs: Some(30),
        }
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers()[header::RETRY_AFTER], "30");
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "application/json");
    }
}
